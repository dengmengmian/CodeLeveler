use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::broadcast;

use leveler_agent::{AdvisoryKind, AgentError, AgentOutcome, StopReason};
use leveler_core::ToolCallId;
use leveler_engine::EngineEvent;

use leveler_client_protocol::{
    AnswerEffect, ChildContribution, FailureCategory, FailureDelivery, FailureRetryability,
    FailureSource, FinalizationStage, MessageId, NotificationLevel, PlanStepStatus, RuntimeEvent,
    UiFailure, UiPlan, UiPlanStep,
};

use crate::AppError;

/// The client projection of a persisted plan table. One mapping for the live
/// event and the reconnect snapshot, so the two cannot disagree on a status.
pub(crate) fn ui_plan(steps: Vec<leveler_lifecycle::PlanStep>) -> UiPlan {
    UiPlan {
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
    }
}

pub(crate) fn turn_runtime_event(result: Result<AgentOutcome, AppError>) -> RuntimeEvent {
    match result {
        Ok(outcome) => turn_end_event(outcome.stop_reason, outcome.stop_detail),
        Err(AppError::Agent(AgentError::Cancelled)) => RuntimeEvent::TurnCancelled,
        Err(AppError::Agent(AgentError::Model(error)))
            if error.kind == leveler_model::ModelErrorKind::Truncated =>
        {
            RuntimeEvent::TurnTruncated {
                error: error.to_string(),
            }
        }
        Err(AppError::UnclosedTerminalBoundary(error)) => RuntimeEvent::Notification {
            level: NotificationLevel::Error,
            message: format!("任务终态尚未发布：收尾证据无法持久化，需要恢复后重新结算（{error}）"),
        },
        Err(AppError::TerminalCommitFailed(error)) => RuntimeEvent::Notification {
            level: NotificationLevel::Error,
            message: format!("任务终态尚未发布：权威终态提交失败，需要恢复后重新结算（{error}）"),
        },
        Err(error) => RuntimeEvent::TurnFailed {
            error: error.to_string(),
            failure: match &error {
                AppError::Model(model) => Some(ui_failure_from_model(model)),
                _ => None,
            },
        },
    }
}

/// Project a typed provider failure onto the product failure contract.
///
/// The ONLY place a `ModelError` becomes a `UiFailure`. Everything above reads
/// the structured fields; the vendor's raw text travels only as `detail`.
pub fn ui_failure_from_model(error: &leveler_model::ModelError) -> UiFailure {
    use leveler_model::{DeliveryState, ModelErrorKind, Retryability};
    // A request that provably never left this process cannot have been rejected
    // by the provider. Only a local resolution/validation failure records
    // `NotSent` with `InvalidRequest`; classifying it as a remote rejection
    // sends the user to debug a service that was never called.
    let local_resolution = error.delivery_state == DeliveryState::NotSent
        && matches!(
            error.kind,
            ModelErrorKind::InvalidRequest | ModelErrorKind::Auth
        );
    let category = if local_resolution {
        FailureCategory::LocalConfiguration
    } else {
        match error.kind {
            ModelErrorKind::Auth => FailureCategory::Authentication,
            ModelErrorKind::InvalidRequest | ModelErrorKind::Decode | ModelErrorKind::Truncated => {
                FailureCategory::InvalidRequest
            }
            ModelErrorKind::RateLimit => FailureCategory::RateLimit,
            ModelErrorKind::ProviderUnavailable | ModelErrorKind::ContentFiltered => {
                FailureCategory::Provider
            }
            ModelErrorKind::Transport | ModelErrorKind::StreamInterrupted => {
                FailureCategory::Network
            }
            ModelErrorKind::Timeout => FailureCategory::Timeout,
            ModelErrorKind::Cancelled => FailureCategory::Cancelled,
            ModelErrorKind::ConversationProtocol | ModelErrorKind::Other => {
                FailureCategory::Internal
            }
        }
    };
    // Source follows the same fact: a locally refused request is the local
    // execution's failure, a refused tool exchange is the runtime's, and every
    // other kind came back from the provider call.
    let source = if local_resolution {
        FailureSource::Local
    } else {
        match error.kind {
            ModelErrorKind::ConversationProtocol => FailureSource::Runtime,
            _ => FailureSource::Provider,
        }
    };
    let retryability = match error.retryability() {
        Retryability::Safe => FailureRetryability::Safe,
        Retryability::Caution => FailureRetryability::Caution,
        Retryability::Unknown => FailureRetryability::Unknown,
        Retryability::Never => FailureRetryability::Never,
    };
    let delivery = match error.delivery_state {
        DeliveryState::NotSent => FailureDelivery::NotSent,
        DeliveryState::SentNoResponse => FailureDelivery::SentNoResponse,
        DeliveryState::Responded => FailureDelivery::Responded,
        DeliveryState::StreamInterrupted { progress } => FailureDelivery::StreamInterrupted {
            text: progress.text,
            tool_args: progress.tool_args,
        },
        DeliveryState::Unknown => FailureDelivery::Unknown,
    };
    // Provider-agnostic product copy. Deliberately generic: the runtime has no
    // evidence for a more specific cause, and inventing one from the raw text
    // would be a guess. The raw text stays in `detail`.
    let summary = if local_resolution {
        // The exact missing provider/model travels in `detail`; the primary
        // line points at the fix and never claims a provider rejection.
        "本地模型配置无效，请求未发送；请在 ~/.leveler/config.toml 配置 provider/model。"
            .to_string()
    } else {
        match category {
            FailureCategory::Authentication => "模型服务拒绝了当前凭据。",
            FailureCategory::InvalidRequest => "模型服务拒绝了当前请求。",
            FailureCategory::LocalConfiguration => "本地模型配置无效，请求未发送。",
            FailureCategory::RateLimit => "模型服务繁忙，请稍后重试。",
            FailureCategory::Provider => "模型服务暂时不可用。",
            FailureCategory::Network => "无法连接模型服务。",
            FailureCategory::Timeout => "模型服务未及时响应。",
            FailureCategory::Cancelled => "请求已取消。",
            _ if error.kind == ModelErrorKind::ConversationProtocol => {
                "内部会话协议错误，请求未发送给模型服务。"
            }
            _ => "请求失败。",
        }
        .to_string()
    };
    UiFailure {
        category,
        source,
        provider: error.provider.clone(),
        model: error.model().map(str::to_string),
        provider_code: error.provider_code().map(str::to_string),
        request_id: error.request_id().map(str::to_string),
        status: error.status,
        retries: error.retry_attempts,
        retryability,
        delivery,
        summary,
        detail: error.message.clone(),
    }
}

/// The turn-end event for a typed stop reason — the terminal marker a person
/// reads at the bottom of a finished turn.
///
/// Split out of [`turn_runtime_event`] so a consumer that holds only the
/// durable `TaskFinished { stop, reason }` — a replay of a recorded session, an
/// audit of one — reaches the same terminal state the live client reached,
/// through this code rather than a second copy of the mapping. Terminal state
/// is where "blocked" is told apart from "done", so a replay that cannot reach
/// it cannot check the one thing that matters most.
pub fn turn_end_event(stop: StopReason, stop_detail: Option<String>) -> RuntimeEvent {
    let detail = stop_detail.filter(|s| !s.trim().is_empty());
    match stop {
        StopReason::Completed => RuntimeEvent::TurnCompleted,
        StopReason::Answered => RuntimeEvent::TurnAnswered,
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
        // A pinned round ceiling fired: bounded work reached its edge.
        // The runtime's own `stop_detail` here is a machine token
        // ("round ceiling reached") that must never reach the screen,
        // so this outcome always speaks in product wording. Not a
        // liftable budget, so do not point at /goal as if more
        // work-window helps.
        StopReason::TurnLimitReached => RuntimeEvent::TurnIncomplete {
            reason: "达到执行回合上限 · 已停止,请检查是否陷入循环".into(),
        },
        StopReason::Blocked => RuntimeEvent::TurnIncomplete {
            reason: detail.unwrap_or_else(|| "目标被标记为阻塞".to_string()),
        },
        StopReason::Stalled => RuntimeEvent::TurnIncomplete {
            reason: detail.unwrap_or_else(|| "goal 未确认完成".into()),
        },
    }
}

/// A write-ownership decision in the words the rest of the UI uses.
///
/// `action` is an audit key and `detail` is "<agent id>: <paths>". Pasted
/// together they filled the status line with
/// "delegation ownership_granted: 5500f56e-0773-…: src/load.js" — an internal
/// name and a UUID where the user looks to see what is happening. The agent
/// that owns the paths is already a row in the roster; the paths are the part
/// worth reading.
fn delegation_stage_label(action: &str, detail: &str) -> String {
    let paths = detail.split_once(": ").map_or(detail, |(_, rest)| rest);
    match action {
        "ownership_granted" => format!("写入权限已分配 · {paths}"),
        "ownership_denied" => format!("写入权限被拒 · {paths}"),
        _ => paths.to_string(),
    }
}

/// A required stage in the words the rest of the UI uses.
///
/// `action` is an audit key (`review_launching`, `analyze_finished_ok`, or a
/// bare `launching` for the closure reviewer) and `detail` is its
/// machine-readable reason. Pasted together they printed
/// "review review_launching: review" at the user — three machine words and no
/// sentence.
fn review_stage_label(action: &str, detail: &str) -> String {
    let (stage, verb) = match action.split_once('_') {
        Some((stage, verb)) => (stage, verb),
        None => ("reviewer", action),
    };
    let stage = match stage {
        "analyze" => "分析",
        "review" | "reviewer" => "评审",
        other => other,
    };
    match verb {
        "launching" => format!("{stage} · 启动"),
        "finished_ok" => format!("{stage} · 完成"),
        "finished_incomplete" => format!("{stage} · 未完成"),
        // The only detail worth carrying: why it could not start.
        "launch_failed" => format!("{stage} · 启动失败：{detail}"),
        _ => stage.to_string(),
    }
}

fn task_finished_event(
    outcome: leveler_lifecycle::TaskOutcome,
    reason: Option<String>,
    failure: Option<leveler_model::ModelError>,
    stop: Option<StopReason>,
    warnings: Vec<String>,
) -> RuntimeEvent {
    let detail_with_warnings = || {
        let mut parts = Vec::new();
        for part in reason.iter().chain(warnings.iter()) {
            if !part.trim().is_empty() && !parts.contains(part) {
                parts.push(part.clone());
            }
        }
        parts.join("; ")
    };
    if outcome == leveler_lifecycle::TaskOutcome::Completed && !warnings.is_empty() {
        return RuntimeEvent::TurnCompletedWithWarnings {
            reason: detail_with_warnings(),
        };
    }
    if outcome == leveler_lifecycle::TaskOutcome::Completed && stop == Some(StopReason::Completed) {
        return RuntimeEvent::TurnCompleted;
    }
    if outcome == leveler_lifecycle::TaskOutcome::Completed && stop.is_none() {
        return RuntimeEvent::TurnCompleted;
    }
    if let Some(stop) = stop {
        return turn_end_event(stop, reason);
    }
    match outcome {
        leveler_lifecycle::TaskOutcome::Interrupted => RuntimeEvent::TurnCancelled,
        // Explicit task cancellation is terminal, not a resumable pause.
        leveler_lifecycle::TaskOutcome::Cancelled => RuntimeEvent::TaskCancelled,
        leveler_lifecycle::TaskOutcome::Failed => RuntimeEvent::TurnFailed {
            error: reason.unwrap_or_else(|| "任务执行失败".to_string()),
            failure: failure.as_ref().map(ui_failure_from_model),
        },
        leveler_lifecycle::TaskOutcome::BudgetLimited => RuntimeEvent::TurnIncomplete {
            reason: reason.unwrap_or_else(|| "执行预算已用尽".to_string()),
        },
        leveler_lifecycle::TaskOutcome::Blocked => RuntimeEvent::TurnIncomplete {
            reason: reason.unwrap_or_else(|| "目标被标记为阻塞".to_string()),
        },
        leveler_lifecycle::TaskOutcome::Completed => RuntimeEvent::TurnAnswered,
    }
}

fn finalization_stage(phase: &str) -> Option<FinalizationStage> {
    Some(match phase {
        "settling_dependencies" => FinalizationStage::SettlingDependencies,
        "settling_turn" => FinalizationStage::SettlingDependencies,
        "review" => FinalizationStage::Review,
        "continuation_checkpoint" => FinalizationStage::ResolvingOutcome,
        "resolving_outcome" => FinalizationStage::ResolvingOutcome,
        "publishing_terminal" => FinalizationStage::PublishingTerminal,
        _ => return None,
    })
}

/// Translates the runtime's synchronous `AgentEvent`s into protocol events. Tool
/// calls carry a stable id, so a `ToolResult` pairs with its `ToolCall` by id
/// (NOT arrival order — read-only tools run in parallel, so results can arrive
/// out of order or after an interleaved serial tool). `tool_starts` records each
/// call's start time by id for the client-side duration.
pub struct EventBridge {
    events: broadcast::Sender<RuntimeEvent>,
    tool_starts: HashMap<String, Instant>,
    /// The in-flight assistant message id, open while deltas stream (spec §16).
    open_assistant: Option<MessageId>,
    /// Recently completed assistant texts this turn, for the near-duplicate
    /// fold (a nudged model repeating its "task complete" summary). Display
    /// layer only — the persisted transcript keeps every message.
    recent_assistant_texts: std::collections::VecDeque<String>,
    /// True while the current model round was opened by a harness closeout
    /// nudge. The fold below applies to THAT round only: the model is
    /// re-answering the message the nudge answered, so a restatement carries
    /// no new narration. In any other round the text is the model's own
    /// commentary, however similar it reads to an earlier message.
    ///
    /// The scope ends at the first of: an assistant message (the round's
    /// answer, consumed below), the round's own tool call (the round answered
    /// with calls, so it has no message left to give), or the next turn.
    round_after_closeout_nudge: bool,
    /// Role per in-flight child, so the terminal event can carry the role the
    /// spawn announced instead of an empty string.
    child_roles: HashMap<String, String>,
    /// A TaskFinished event is the one terminal authority. Once projected,
    /// post-terminal timing/cleanup events can never move the client back to a
    /// busy state, and the interactive wrapper knows not to emit a duplicate.
    terminal_published: bool,
    /// The host publishes the durable terminal while holding its admission
    /// lock, then releases that admission before consumers can observe it.
    terminal_publisher: Option<Box<dyn FnOnce(RuntimeEvent) + Send>>,
    /// The runtime's active-turn table plus this stream's session: every
    /// forwarded event stamps observable activity on the running turn. One
    /// instrumentation point for every engine event rather than one per call
    /// site, and it can only ever DESCRIBE activity — it never cancels or
    /// authorizes anything.
    progress: Option<(
        std::sync::Arc<crate::active_turns::ActiveTurns>,
        leveler_core::SessionId,
    )>,
}

/// The wire spelling of the runtime's four-way child reading.
pub(crate) fn project_child_outcome(
    outcome: leveler_lifecycle::ChildStatus,
) -> leveler_client_protocol::ChildOutcome {
    use leveler_client_protocol::ChildOutcome as Wire;
    use leveler_lifecycle::ChildStatus;
    match outcome {
        ChildStatus::CompletedWithFindings => Wire::CompletedWithFindings,
        ChildStatus::CompletedNoFindings => Wire::CompletedNoFindings,
        ChildStatus::IncompletePartial => Wire::IncompletePartial,
        ChildStatus::IncompleteNoResult => Wire::IncompleteNoResult,
    }
}

/// The wire spelling of how a child's activation ended.
pub(crate) fn project_child_stop(
    stop: leveler_lifecycle::ChildStop,
) -> leveler_client_protocol::ChildStop {
    use leveler_client_protocol::ChildStop as Wire;
    use leveler_lifecycle::ChildStop;
    match stop {
        ChildStop::Completed => Wire::Completed,
        ChildStop::Incomplete => Wire::Incomplete,
        ChildStop::Budget => Wire::Budget,
        ChildStop::Cancelled => Wire::Cancelled,
        ChildStop::Failed => Wire::Failed,
        ChildStop::Lost => Wire::Lost,
    }
}

/// The wire spelling of the runtime's typed limit for a budget stop. Carried
/// so a client can say a wall-clock timeout instead of folding every budget
/// stop into one word.
pub(crate) fn project_child_limit(
    limit: leveler_lifecycle::ChildLimit,
) -> leveler_client_protocol::ChildLimit {
    use leveler_client_protocol::ChildLimit as Wire;
    use leveler_lifecycle::ChildLimit;
    match limit {
        ChildLimit::Duration => Wire::Duration,
        ChildLimit::ModelTokens => Wire::ModelTokens,
        ChildLimit::Cost => Wire::Cost,
        ChildLimit::Commands => Wire::Commands,
        ChildLimit::ModifiedFiles => Wire::ModifiedFiles,
        ChildLimit::RoundWindow => Wire::RoundWindow,
        ChildLimit::RoundCeiling => Wire::RoundCeiling,
    }
}

/// Map the runtime's projection onto the wire type.
///
/// Deliberately total: every field crosses. Dropping one here is invisible at
/// the call site and unrecoverable downstream — which is exactly how
/// `contribution` was lost before.
fn project_contribution(c: &leveler_lifecycle::ChildResultProjection) -> ChildContribution {
    ChildContribution {
        role: c.role.clone(),
        profile_id: c.profile_id.clone(),
        profile_role: c.profile_role.clone(),
        read_only: c.read_only,
        findings_total: c.findings_total,
    }
}

/// How many completed texts the fold compares against. Nudge rounds can carry
/// a short tool-status text between two copies of the summary, so comparing
/// only the immediately previous message would miss the repeat.
const FOLD_LOOKBACK: usize = 4;

/// Minimum normalized length before the fold may apply: short acknowledgements
/// repeat legitimately and must stay visible.
const FOLD_MIN_CHARS: usize = 24;

/// Fraction of the new text's trigrams that must already exist in an earlier
/// text for the new one to count as "nothing new". A re-stated summary with a
/// trivial suffix lands ≈0.87; an answer with a genuinely new paragraph drops
/// below ≈0.7 — 0.85 separates the two with margin on the keep side.
const FOLD_CONTAINMENT: f64 = 0.85;

/// The harness key of a closeout continuation nudge, as spelled by
/// `leveler_agent::RuntimeInjectionKind::as_key`'s `CloseoutNudge` arm
/// (`closeout_goal_unresolved` / `closeout_empty_answer`). This is the owner's
/// real scope: a round the harness opened by re-driving a quiet turn. It is a
/// lifecycle key chosen by the harness, never anything read out of the model's
/// prose.
fn opens_from_closeout_nudge(kind: &str) -> bool {
    kind.starts_with("closeout_")
}

/// True when `new` adds (nearly) nothing over `prev`: compare character
/// trigrams of the normalized texts and require [`FOLD_CONTAINMENT`] of the
/// new text's trigrams to be already present. Containment (not symmetric
/// similarity) so a shorter re-statement of a long summary still folds.
fn is_near_duplicate(prev: &str, new: &str) -> bool {
    fn normalized(text: &str) -> Vec<char> {
        text.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }
    fn trigrams(chars: &[char]) -> std::collections::HashSet<[char; 3]> {
        chars.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
    }
    let p = normalized(prev);
    let n = normalized(new);
    if p.len() < FOLD_MIN_CHARS || n.len() < FOLD_MIN_CHARS {
        return false;
    }
    let new_grams = trigrams(&n);
    if new_grams.is_empty() {
        return false;
    }
    let prev_grams = trigrams(&p);
    let overlap = new_grams.iter().filter(|g| prev_grams.contains(*g)).count();
    overlap as f64 / new_grams.len() as f64 >= FOLD_CONTAINMENT
}

/// The harness's closeout continuation, spelled exactly as
/// `AgentEvent::runtime_injection` spells it (`RuntimeInjectionKind::as_key`).
/// That injection is what opens the one round the near-duplicate fold is
/// allowed to touch.
#[cfg(test)]
fn closeout_nudge() -> EngineEvent {
    EngineEvent::RuntimeInjection {
        kind: "closeout_goal_unresolved".into(),
        role: "user".into(),
        model_step: 2,
        forces_continuation: true,
    }
}

impl EventBridge {
    /// The runtime's one classification of what a call does to the answer.
    ///
    /// Stated here, at the boundary where the runtime's facts become client
    /// facts, using the tool vocabulary's single owner. Every surface reads the
    /// stamped value: re-deciding it per renderer is what made `FinalAnswer` a
    /// second truth source. An unclassified call is `Work` (see
    /// [`leveler_tools::acts_on_answer`]).
    fn answer_effect(name: &str, arguments: &str) -> AnswerEffect {
        if leveler_tools::acts_on_answer(name, arguments) {
            AnswerEffect::Work
        } else {
            AnswerEffect::Bookkeeping
        }
    }

    pub fn new(events: broadcast::Sender<RuntimeEvent>) -> Self {
        Self {
            events,
            tool_starts: HashMap::new(),
            open_assistant: None,
            recent_assistant_texts: std::collections::VecDeque::new(),
            round_after_closeout_nudge: false,
            child_roles: HashMap::new(),
            terminal_published: false,
            terminal_publisher: None,
            progress: None,
        }
    }

    /// Stamp observable activity on the turn behind this stream for every
    /// forwarded event. A no-op once the turn has left the active table.
    pub(crate) fn with_progress(
        mut self,
        active: std::sync::Arc<crate::active_turns::ActiveTurns>,
        session_id: leveler_core::SessionId,
    ) -> Self {
        self.progress = Some((active, session_id));
        self
    }

    pub fn with_terminal_publisher(
        mut self,
        publisher: impl FnOnce(RuntimeEvent) + Send + 'static,
    ) -> Self {
        self.terminal_publisher = Some(Box::new(publisher));
        self
    }

    pub fn terminal_published(&self) -> bool {
        self.terminal_published
    }

    pub fn forward(&mut self, event: EngineEvent) {
        // TaskFinished is the closed event boundary. No event from the old
        // epoch may project after it and race a newly admitted turn.
        if self.terminal_published {
            return;
        }
        if let Some((active, session_id)) = &self.progress {
            active.touch(session_id);
        }
        match event {
            EngineEvent::FinalizationStarted { .. } => {
                if !self.terminal_published {
                    let _ = self.events.send(RuntimeEvent::TurnFinalizing {
                        stage: FinalizationStage::SettlingDependencies,
                    });
                }
            }
            EngineEvent::FinalizationPhaseStarted { phase, .. } => {
                if !self.terminal_published
                    && let Some(stage) = finalization_stage(&phase)
                {
                    let _ = self.events.send(RuntimeEvent::TurnFinalizing { stage });
                }
            }
            EngineEvent::FinalizationPhaseFinished { .. } => {}
            EngineEvent::StreamAttemptStarted => {
                let message_id = self.open_assistant.take();
                let _ = self
                    .events
                    .send(RuntimeEvent::AssistantAttemptReset { message_id });
            }
            EngineEvent::AssistantDelta { text: delta } => {
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
            EngineEvent::ReasoningDelta { text: delta } => {
                let _ = self.events.send(RuntimeEvent::ReasoningDelta { delta });
            }
            EngineEvent::RuntimeInjection { kind, .. } => {
                // The ONE lifecycle opening that scopes the fold below: the
                // harness re-drove a quiet round, so the next assistant text
                // re-states what the nudge answered.
                self.round_after_closeout_nudge = opens_from_closeout_nudge(&kind);
            }
            EngineEvent::TurnStarted { .. } => {
                // A nudge whose round never produced a message must not follow
                // the reader into the next turn.
                self.round_after_closeout_nudge = false;
            }
            EngineEvent::AssistantMessage { text } => {
                // Near-duplicate fold: a nudged model that re-states the
                // summary the nudge answered is collapsed into one notice
                // instead of rendering the repeat. Display only — the
                // transcript sink keeps the message.
                //
                // Scoped to the nudge-opened round: eaten unconditionally, so
                // it can cover exactly one message. Ordinary narration — the
                // model's own progress text — is never folded for resembling
                // an earlier message.
                let duplicate = std::mem::take(&mut self.round_after_closeout_nudge)
                    && self
                        .recent_assistant_texts
                        .iter()
                        .any(|prev| is_near_duplicate(prev, &text));
                if duplicate {
                    if let Some(id) = self.open_assistant.take() {
                        // Streamed path: the deltas are already on screen —
                        // retract the unfinished block by id.
                        let _ = self.events.send(RuntimeEvent::AssistantAttemptReset {
                            message_id: Some(id),
                        });
                    }
                    let _ = self.events.send(RuntimeEvent::Notification {
                        level: NotificationLevel::Info,
                        message: "重复的总结已折叠(内容与先前一致)".to_string(),
                    });
                    return;
                }
                if !text.trim().is_empty() {
                    self.recent_assistant_texts.push_back(text.clone());
                    if self.recent_assistant_texts.len() > FOLD_LOOKBACK {
                        self.recent_assistant_texts.pop_front();
                    }
                }
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
            // A delegated agent's canonical tool events are durable recovery
            // facts, not a second UI stream: the parent already surfaces child
            // work as attributed SubAgentActivity. Projecting them here would
            // render every child call twice.
            EngineEvent::ToolCallStarted {
                agent_id: Some(_), ..
            }
            | EngineEvent::ToolCallFinished {
                agent_id: Some(_), ..
            } => {}
            EngineEvent::ToolCallStarted {
                call_id: id,
                name,
                arguments,
                parallel,
                risk: _,
                agent_id: None,
                model_step,
            } => {
                // A tool call ends the current assistant thought. Close any open
                // streamed message so the next round's text opens a fresh block
                // instead of being concatenated onto this one.
                if let Some(open) = self.open_assistant.take() {
                    let _ = self
                        .events
                        .send(RuntimeEvent::AssistantMessageCompleted { message_id: open });
                }
                // The nudge-opened round answered with calls, not text: any
                // message the round had was emitted above, before its tools
                // run. End the fold scope here so the NEXT round's progress is
                // never mistaken for the nudge response.
                self.round_after_closeout_nudge = false;
                self.tool_starts.insert(id.clone(), Instant::now());
                let answer_effect = Self::answer_effect(&name, &arguments);
                let _ = self.events.send(RuntimeEvent::ToolCallStarted {
                    id: ToolCallId::new(id),
                    name,
                    arguments,
                    parallel,
                    model_step,
                    answer_effect: Some(answer_effect),
                });
            }
            EngineEvent::ToolCallFinished {
                call_id: id,
                name,
                is_error,
                preview,
                agent_id: None,
                applied_diff,
                exit_code,
                stop,
            } => {
                // Pair with the ToolCall by id, whatever order results arrive in.
                // A denial/guard result has no prior ToolCall — synthesize a
                // started block first so it still renders and isn't dropped.
                // Such a result is still this round's own call, so it also ends
                // the nudge fold scope (see the ToolCallStarted arm).
                self.round_after_closeout_nudge = false;
                let start = match self.tool_starts.remove(&id) {
                    Some(start) => start,
                    None => {
                        // Same for the answer: the runtime recorded no
                        // arguments with this result, so the call is
                        // classified at the name level. That is what the
                        // reference implementation did here too — an
                        // argument-dependent call with no arguments is work.
                        let answer_effect = Self::answer_effect(&name, "");
                        let _ = self.events.send(RuntimeEvent::ToolCallStarted {
                            id: ToolCallId::new(id.clone()),
                            name,
                            arguments: String::new(),
                            parallel: false,
                            // No round identity survived with this result:
                            // a client keeps its old grouping rather than
                            // invent a boundary (see the arm above).
                            model_step: None,
                            answer_effect: Some(answer_effect),
                        });
                        Instant::now()
                    }
                };
                let _ = self.events.send(RuntimeEvent::ToolCallCompleted {
                    id: ToolCallId::new(id),
                    ok: !is_error,
                    preview,
                    duration_ms: start.elapsed().as_millis() as u64,
                    applied_diff,
                    exit_code,
                    stop: stop.map(|stop| match stop {
                        leveler_execution::CommandStop::Confirmed => {
                            leveler_client_protocol::UiCommandStop::Confirmed
                        }
                        leveler_execution::CommandStop::Unconfirmed => {
                            leveler_client_protocol::UiCommandStop::Unconfirmed
                        }
                    }),
                });
            }
            EngineEvent::ToolCallOutput {
                call_id,
                stream,
                chunk,
            } => {
                let _ = self.events.send(RuntimeEvent::ToolCallOutput {
                    id: ToolCallId::new(call_id),
                    stream,
                    chunk,
                });
            }
            EngineEvent::WorkspaceSnapshotCreated { .. } => {
                // Durability metadata is persisted by the engine; it has no
                // standalone transcript cell in the TUI.
            }
            EngineEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                reasoning_tokens,
            } => {
                let _ = self.events.send(RuntimeEvent::TokenUsage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                    reasoning_tokens,
                });
            }
            EngineEvent::ContextUsage { accounting } => {
                let _ = self.events.send(RuntimeEvent::ContextUsage { accounting });
            }
            EngineEvent::Compacted { from, to } => {
                // Structured fact — clients own the wording and locale.
                let _ = self.events.send(RuntimeEvent::ContextCompacted {
                    from: from as u32,
                    to: to as u32,
                });
            }
            // Reachable again under the canonical projection: the legacy
            // shim dropped this durable fact before the arm could run.
            EngineEvent::ContextExpanded {
                from, to, reason, ..
            } => {
                let _ = self.events.send(RuntimeEvent::ContextExpanded {
                    from_tokens: from,
                    to_tokens: to,
                    reason,
                });
            }
            EngineEvent::ContextSnapshot { .. } => {
                // Engine durability metadata; no standalone UI cell.
            }
            EngineEvent::PlanUpdated { steps } => {
                let plan = ui_plan(steps);
                let _ = self.events.send(RuntimeEvent::PlanUpdated { plan });
            }
            EngineEvent::MemoryRecalled { count, ids } => {
                // Structured, so no client parses the injected system block.
                let _ = self
                    .events
                    .send(RuntimeEvent::MemoryRecalled { count, ids });
            }
            EngineEvent::MemoryChanged {
                operation,
                id,
                title,
                authority,
            } => {
                let _ = self.events.send(RuntimeEvent::MemoryChanged {
                    operation,
                    id,
                    title,
                    authority,
                });
            }
            EngineEvent::GoalIntercepted { kind, detail } => {
                // Surface as activity label; full tool error remains the model path.
                let _ = self.events.send(RuntimeEvent::AgentActivity {
                    label: format!("gate refused {kind}: {detail}"),
                });
            }
            EngineEvent::DelegationStage { action, detail } => {
                // Durable ownership-provenance fact; surfaced as a light
                // activity label so a grant/denial is visible live.
                let _ = self.events.send(RuntimeEvent::AgentActivity {
                    label: delegation_stage_label(&action, &detail),
                });
            }
            EngineEvent::ReviewStage {
                required,
                action,
                detail,
            } => {
                // A required review's fate is user-relevant; the rest is audit
                // trail only (durable, not surfaced).
                if required {
                    let _ = self.events.send(RuntimeEvent::AgentActivity {
                        label: review_stage_label(&action, &detail),
                    });
                }
            }
            EngineEvent::EvidenceLedgerUpdated { .. } => {
                // Persisted by engine; no dedicated UI cell in v1.
            }
            EngineEvent::GoalCheckpointCreated {
                checkpoint_id,
                goal_id,
                reason,
                created_at,
                payload,
            } => {
                let _ = self.events.send(RuntimeEvent::GoalRecapCreated {
                    recap: crate::goal_recap::project_goal_recap_parts(
                        &checkpoint_id,
                        &goal_id,
                        &reason,
                        &created_at,
                        &payload,
                    ),
                });
            }
            EngineEvent::UserShellStarted {
                execution_id,
                command,
                cwd,
            } => {
                let _ = self.events.send(RuntimeEvent::UserShellStarted {
                    execution_id,
                    command,
                    cwd,
                });
            }
            EngineEvent::UserShellOutput {
                execution_id,
                stream,
                chunk,
            } => {
                let _ = self.events.send(RuntimeEvent::UserShellOutput {
                    execution_id,
                    stream,
                    chunk,
                });
            }
            EngineEvent::UserShellFinished {
                execution_id,
                exit_code,
                duration_ms,
                status,
            } => {
                let _ = self.events.send(RuntimeEvent::UserShellExited {
                    execution_id,
                    exit_code,
                    duration_ms,
                    status,
                });
            }
            EngineEvent::AdvisoryStarted { kind } => {
                // Closeout round trips that happen after the visible answer.
                // Label them so the status line does not read "等待模型" with no
                // hint of why the wait continues. Unknown keys (older/newer
                // logs) degrade to the audit label.
                use leveler_agent::closeout::CloseoutReason;
                let kind = AdvisoryKind::from_key(&kind).unwrap_or(AdvisoryKind::ContextCompaction);
                let label = match kind {
                    AdvisoryKind::ContextCompaction => "压缩上下文中…",
                    AdvisoryKind::CloseoutNudge(reason) => match reason {
                        CloseoutReason::GoalUnresolved => "催办:未调用 update_goal,再询一轮",
                        CloseoutReason::EmptyAnswer => "催办:上轮回答为空,再询一轮",
                    },
                };
                let _ = self.events.send(RuntimeEvent::AgentActivity {
                    label: label.to_string(),
                });
            }
            EngineEvent::CommandProgress { label, elapsed_ms } => {
                // Structured event; the TUI reducer turns it into the status-line
                // label ("运行 cargo test · 02:31"). Single source, so Web/logs get
                // the same structured data instead of a pre-formatted string.
                let _ = self
                    .events
                    .send(RuntimeEvent::CommandProgress { label, elapsed_ms });
            }
            EngineEvent::ModelRetrying {
                attempt,
                max_attempts,
                delay_ms,
            } => {
                // Connectivity is ephemeral: a status-line hint, never a
                // transcript item. Structured so every client renders it in its
                // own vocabulary.
                let _ = self.events.send(RuntimeEvent::ModelRetrying {
                    attempt,
                    max_attempts,
                    delay_ms,
                });
            }
            // Supervisor control state. Durable for recovery, not for display:
            // the window count and the guards behind it are how the runtime
            // decides, and the user already sees the decision.
            EngineEvent::WindowStateUpdated { .. } => {}
            EngineEvent::ProgressUpdated { ledger } => {
                let phase = match ledger.phase {
                    leveler_lifecycle::TurnPhase::Active => "active",
                    leveler_lifecycle::TurnPhase::AwaitingModel => "awaiting_model",
                    leveler_lifecycle::TurnPhase::ToolBatch => "tool_batch",
                    leveler_lifecycle::TurnPhase::Closing => "closing",
                    leveler_lifecycle::TurnPhase::AwaitingUser => "awaiting_user",
                    leveler_lifecycle::TurnPhase::Closed => "closed",
                };
                let _ = self.events.send(RuntimeEvent::TurnProgress {
                    phase: phase.to_string(),
                    closing: ledger.closing,
                    no_progress_streak: ledger.no_progress_streak,
                });
                if ledger.closing {
                    let _ = self.events.send(RuntimeEvent::AgentActivity {
                        label: "计划已完成 · 收口中".into(),
                    });
                } else if ledger.no_progress_streak > 0 {
                    let _ = self.events.send(RuntimeEvent::AgentActivity {
                        label: format!("无进展 streak {}", ledger.no_progress_streak),
                    });
                }
            }
            EngineEvent::SubAgentStarted {
                id,
                nickname,
                role,
                task,
                profile_id,
                profile_role,
                read_only,
                spec,
            } => {
                // The capability contract travels with the child so the UI can
                // state what it was allowed to do rather than implying it.
                self.child_roles.insert(id.clone(), role.clone());
                let spec = spec.unwrap_or_default();
                let _ = self.events.send(RuntimeEvent::SubAgentUpdated {
                    id,
                    nickname,
                    role,
                    title: spec.title.clone(),
                    done: false,
                    ok: false,
                    detail: task,
                    profile_id,
                    profile_role,
                    read_only,
                    agent: crate::agents::child_agent_identity(&spec),
                    contribution: None,
                    outcome: None,
                    stop: None,
                    // No bound has fired while the child is still running.
                    limit: None,
                    background: Some(spec.background),
                    scope: spec.files,
                });
            }
            EngineEvent::SubAgentProgress {
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
            EngineEvent::SubAgentFinished {
                id,
                nickname,
                ok,
                summary,
                contribution,
                outcome,
                stop,
                limit,
                ..
            } => {
                let projected = contribution.as_ref().map(project_contribution);
                // Prefer the role recorded at spawn; a projection carries it
                // too, but a child that was never announced has neither and an
                // empty string is honest about that.
                let role = self
                    .child_roles
                    .remove(&id)
                    .or_else(|| {
                        projected
                            .as_ref()
                            .map(|c: &ChildContribution| c.role.clone())
                    })
                    .unwrap_or_default();
                let _ = self.events.send(RuntimeEvent::SubAgentUpdated {
                    id,
                    nickname,
                    role,
                    // The title was fixed at spawn; a terminal does not restate it.
                    title: None,
                    done: true,
                    ok,
                    detail: summary,
                    profile_id: projected.as_ref().and_then(|c| c.profile_id.clone()),
                    profile_role: projected.as_ref().and_then(|c| c.profile_role.clone()),
                    read_only: projected.as_ref().is_some_and(|c| c.read_only),
                    agent: None,
                    contribution: projected,
                    outcome: outcome.map(project_child_outcome),
                    stop: stop.map(project_child_stop),
                    limit: limit.map(project_child_limit),
                    background: None,
                    scope: Vec::new(),
                });
            }
            // A child's own transcript is its durable session, not a live
            // client fact: clients see the child through its lifecycle and
            // activity events, never its raw context.
            EngineEvent::SubAgentTranscriptAppended { .. } => {}
            // Written at a window boundary (the reaper, a turn start). A client
            // already holding the child moves it; one that just connected
            // reads the same state from the snapshot's children.
            EngineEvent::SubAgentInterrupted { id } => {
                let _ = self.events.send(RuntimeEvent::SubAgentStateChanged {
                    id,
                    state: leveler_client_protocol::UiChildState::Interrupted,
                });
            }
            EngineEvent::SubAgentResumed { id, .. } => {
                let _ = self.events.send(RuntimeEvent::SubAgentStateChanged {
                    id,
                    state: leveler_client_protocol::UiChildState::Running,
                });
            }
            EngineEvent::SubAgentActivity {
                id,
                phase,
                tool,
                preview,
                is_error,
            } => {
                let _ = self.events.send(RuntimeEvent::SubAgentActivity {
                    id,
                    phase,
                    tool,
                    preview,
                    is_error,
                });
            }
            EngineEvent::RunFinished { .. } => {
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
            // Engine-only facts: persisted in the event log and surfaced by
            // engine-aware consumers (snapshot, approval channel, eval, the
            // parallel strategy). Deliberately NOT on the client event stream.
            // This list is exhaustive on purpose — a new EngineEvent variant
            // must make an explicit projection decision here to compile.
            EngineEvent::TaskFinished {
                outcome,
                reason,
                failure,
                stop,
                warnings,
            } => {
                if !self.terminal_published {
                    self.terminal_published = true;
                    let event = task_finished_event(outcome, reason, failure, stop, warnings);
                    if let Some(publish) = self.terminal_publisher.take() {
                        publish(event);
                    } else {
                        let _ = self.events.send(event);
                    }
                }
            }
            // A tool call's clock starts when the model asks for it, which for
            // an approved command counts the user reading the overlay as time
            // the command ran. The moment the call is allowed is when it can
            // begin, so that is when its clock starts.
            EngineEvent::ApprovalResolved {
                call_id: Some(call_id),
                ..
            } => {
                if let std::collections::hash_map::Entry::Occupied(mut slot) =
                    self.tool_starts.entry(call_id)
                {
                    slot.insert(Instant::now());
                }
            }
            EngineEvent::TaskStarted { .. }
            | EngineEvent::TurnFinished { .. }
            | EngineEvent::ApprovalRequested { .. }
            | EngineEvent::ApprovalResolved { .. }
            | EngineEvent::ClarificationRequested { .. }
            | EngineEvent::ClarificationAnswered { .. }
            | EngineEvent::AcceptanceEvidence { .. }
            | EngineEvent::PhaseChanged { .. }
            | EngineEvent::RequirementReady { .. }
            | EngineEvent::ContextReady { .. }
            | EngineEvent::PlanReady { .. }
            | EngineEvent::NodeStarted { .. }
            | EngineEvent::NodeFinished { .. }
            | EngineEvent::CandidateStarted { .. }
            | EngineEvent::CandidateFinished { .. }
            | EngineEvent::ReviewStarted { .. }
            | EngineEvent::ReviewFinding { .. }
            | EngineEvent::ReviewFailed { .. }
            | EngineEvent::ReviewFinished { .. } => {}
        }
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;

    /// Legacy-vocabulary test helper: these tests predate the canonical
    /// projection and speak AgentEvent; the total `From<AgentEvent> for
    /// EngineEvent` conversion keeps them meaningful unchanged.
    fn forward_agent(bridge: &mut EventBridge, event: leveler_agent::AgentEvent) {
        bridge.forward(event.into());
    }

    fn drain(rx: &mut broadcast::Receiver<RuntimeEvent>) -> Vec<RuntimeEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// The provider raw body becomes `detail`, never the primary summary; the
    /// category and delivery are carried structurally so no client parses the
    /// message.
    #[test]
    fn a_model_error_projects_to_a_structured_failure() {
        use leveler_model::{ModelError, ModelErrorKind};
        let error = ModelError::from_status(400, r#"{"error":{"message":"bad schema"}}"#)
            .with_provider("moonshot");
        assert_eq!(error.kind, ModelErrorKind::InvalidRequest);
        let failure = ui_failure_from_model(&error);
        assert_eq!(failure.category, FailureCategory::InvalidRequest);
        assert_eq!(failure.source, FailureSource::Provider);
        assert_eq!(failure.provider.as_deref(), Some("moonshot"));
        assert_eq!(failure.status, Some(400));
        assert_eq!(failure.retryability, FailureRetryability::Never);
        assert_eq!(failure.delivery, FailureDelivery::Responded);
        assert!(failure.detail.contains("bad schema"));
        assert!(
            !failure.summary.contains("bad schema"),
            "the summary is product copy, not the raw body: {}",
            failure.summary
        );
    }

    /// The retry count the logical lifecycle spent rides on the failure, so a
    /// client can state how long the runtime tried before giving up.
    #[test]
    fn a_terminal_failure_carries_the_spent_retry_count() {
        use leveler_model::{ModelError, ModelErrorKind};
        let error = ModelError::new(ModelErrorKind::Transport, "connection closed")
            .with_provider("deepseek")
            .with_retry_attempts(10);
        let failure = ui_failure_from_model(&error);
        assert_eq!(
            failure.retries,
            Some(10),
            "an exhausted retry loop reports how many it spent"
        );
        // An error that never reached a retry loop states no count rather than
        // inventing a zero.
        let direct = ui_failure_from_model(&ModelError::new(ModelErrorKind::Other, "x"));
        assert_eq!(direct.retries, None);
    }

    /// The vendor's own code, the model addressed, and the correlation id are
    /// part of the structured failure — a report must not have to parse the
    /// raw body to name them.
    #[test]
    fn provider_code_model_and_request_id_project_into_the_failure() {
        use leveler_model::{ModelError, ModelErrorKind};
        let error = ModelError::new(ModelErrorKind::InvalidRequest, "bad schema")
            .with_status(400)
            .with_provider("kimi")
            .with_model("k3")
            .with_provider_code("invalid_request_error")
            .with_request_id("req_abc");
        let failure = ui_failure_from_model(&error);
        assert_eq!(failure.provider.as_deref(), Some("kimi"));
        assert_eq!(failure.model.as_deref(), Some("k3"));
        assert_eq!(
            failure.provider_code.as_deref(),
            Some("invalid_request_error")
        );
        assert_eq!(failure.request_id.as_deref(), Some("req_abc"));
        assert_eq!(failure.status, Some(400));
    }

    /// A request CodeLeveler refused to send because it broke the
    /// tool-exchange invariant is an internal defect. It must not read as
    /// "模型服务拒绝了当前请求" with a provider subtitle — the provider never
    /// saw it.
    #[test]
    fn a_refused_tool_exchange_reads_as_an_internal_protocol_error() {
        use leveler_model::{DeliveryState, ModelError, ModelErrorKind};
        let error = ModelError::new(
            ModelErrorKind::ConversationProtocol,
            "internal conversation protocol error: orphan tool result",
        )
        .with_delivery_state(DeliveryState::NotSent)
        .with_provider("deepseek")
        .with_model("deepseek-flash");
        let failure = ui_failure_from_model(&error);
        assert_eq!(failure.category, FailureCategory::Internal);
        assert!(!failure.category.is_provider());
        assert_eq!(failure.source, FailureSource::Runtime);
        assert_eq!(failure.retryability, FailureRetryability::Never);
        assert_eq!(failure.delivery, FailureDelivery::NotSent);
        assert_eq!(failure.summary, "内部会话协议错误，请求未发送给模型服务。");
        assert!(failure.detail.contains("orphan tool result"));
    }

    /// A local provider-resolution failure must read as a local configuration
    /// problem. The request never left, so no client may report it as a
    /// provider rejection — this was the audit's confirmed misattribution.
    #[test]
    fn an_unknown_provider_is_a_local_failure_not_a_provider_rejection() {
        use leveler_client_protocol::FailureDelivery;
        use leveler_model::{DeliveryState, ModelError, ModelErrorKind};
        let error = ModelError::new(
            ModelErrorKind::InvalidRequest,
            "unknown provider `deepseek`",
        )
        .with_delivery_state(DeliveryState::NotSent)
        .with_provider("deepseek")
        .with_model("deepseek-flash");
        let failure = ui_failure_from_model(&error);
        assert_eq!(failure.category, FailureCategory::LocalConfiguration);
        assert_eq!(failure.source, FailureSource::Local);
        assert_eq!(failure.delivery, FailureDelivery::NotSent);
        assert!(
            !failure.summary.contains("模型服务拒绝"),
            "a request that never left cannot be a provider rejection: {}",
            failure.summary
        );
        assert!(failure.detail.contains("unknown provider"));
    }

    /// The same fact covers an unresolved model id, which fails at the same
    /// local resolution boundary.
    #[test]
    fn an_unknown_model_is_a_local_failure() {
        use leveler_model::{DeliveryState, ModelError, ModelErrorKind};
        let error = ModelError::new(
            ModelErrorKind::InvalidRequest,
            "unknown model `deepseek/does-not-exist`",
        )
        .with_delivery_state(DeliveryState::NotSent)
        .with_provider("deepseek")
        .with_model("does-not-exist");
        let failure = ui_failure_from_model(&error);
        assert_eq!(failure.category, FailureCategory::LocalConfiguration);
        assert_eq!(failure.source, FailureSource::Local);
    }

    /// A genuine remote 400 is a different fact: the provider answered, so it
    /// stays an invalid-request provider rejection.
    #[test]
    fn a_remote_invalid_request_is_still_a_provider_rejection() {
        let error = leveler_model::ModelError::from_status(400, "Model not found")
            .with_provider("deepseek");
        let failure = ui_failure_from_model(&error);
        assert_eq!(failure.category, FailureCategory::InvalidRequest);
        assert_eq!(failure.source, FailureSource::Provider);
        assert_eq!(
            failure.delivery,
            leveler_client_protocol::FailureDelivery::Responded
        );
        assert_eq!(failure.summary, "模型服务拒绝了当前请求。");
    }

    /// A connection that never sent the request is a provider/network outage,
    /// not an invalid request and not a local configuration error.
    #[test]
    fn a_connection_refusal_stays_a_provider_failure() {
        use leveler_model::{DeliveryState, ModelError, ModelErrorKind};
        let error = ModelError::new(ModelErrorKind::ProviderUnavailable, "connection refused")
            .with_delivery_state(DeliveryState::NotSent)
            .with_provider("deepseek");
        let failure = ui_failure_from_model(&error);
        assert_eq!(failure.category, FailureCategory::Provider);
        assert_eq!(failure.source, FailureSource::Provider);
        assert_eq!(
            failure.delivery,
            leveler_client_protocol::FailureDelivery::NotSent
        );
    }

    /// A completion status maps to auth/timeout semantics without string work.
    #[test]
    fn status_codes_keep_their_category() {
        let auth = ui_failure_from_model(&leveler_model::ModelError::from_status(401, "nope"));
        assert_eq!(auth.category, FailureCategory::Authentication);
        let boom = ui_failure_from_model(&leveler_model::ModelError::new(
            leveler_model::ModelErrorKind::Timeout,
            "deadline elapsed",
        ));
        assert_eq!(boom.category, FailureCategory::Timeout);
    }

    /// Regression guard for the confirmed misattribution: no local failure may
    /// ever render the remote-rejection sentence, whatever its detail says.
    #[test]
    fn not_sent_never_claims_a_provider_rejection() {
        use leveler_model::{DeliveryState, ModelError, ModelErrorKind};
        for kind in [
            ModelErrorKind::InvalidRequest,
            ModelErrorKind::Auth,
            ModelErrorKind::ProviderUnavailable,
        ] {
            let error = ModelError::new(kind, "unknown provider `deepseek`")
                .with_delivery_state(DeliveryState::NotSent);
            let failure = ui_failure_from_model(&error);
            assert!(
                !failure.summary.contains("模型服务拒绝"),
                "{kind:?} with NotSent rendered a provider rejection: {}",
                failure.summary
            );
        }
    }

    /// A failed turn's structured failure survives the durable `TaskFinished`
    /// projection, so live and replay reach the same presentation.
    #[test]
    fn a_failed_task_carries_the_structured_failure_to_the_client() {
        let (tx, mut rx) = broadcast::channel(4);
        let mut bridge = EventBridge::new(tx);
        let model = leveler_model::ModelError::from_status(400, "bad").with_provider("moonshot");
        bridge.forward(EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Failed,
            reason: Some("execution error: model error [InvalidRequest]: bad".into()),
            failure: Some(model),
            stop: None,
            warnings: Vec::new(),
        });
        match rx.try_recv() {
            Ok(RuntimeEvent::TurnFailed { failure, .. }) => {
                let failure = failure.expect("structured failure must be projected");
                assert_eq!(failure.category, FailureCategory::InvalidRequest);
                assert_eq!(failure.provider.as_deref(), Some("moonshot"));
            }
            other => panic!("expected a structured TurnFailed, got {other:?}"),
        }
    }

    /// Closeout advisory calls (completeness audit / compaction) must surface as
    /// a labeled AgentActivity so the status line names the wait instead of a
    /// bare "waiting for model".
    #[test]
    fn advisory_started_becomes_a_labeled_activity() {
        for (kind, needle) in [
            (leveler_agent::AdvisoryKind::ContextCompaction, "压缩"),
            (
                leveler_agent::AdvisoryKind::CloseoutNudge(
                    leveler_agent::closeout::CloseoutReason::GoalUnresolved,
                ),
                "update_goal",
            ),
        ] {
            let (tx, mut rx) = broadcast::channel(16);
            let mut bridge = EventBridge::new(tx);
            forward_agent(
                &mut bridge,
                leveler_agent::AgentEvent::AdvisoryStarted { kind },
            );
            let labels: Vec<String> = drain(&mut rx)
                .into_iter()
                .filter_map(|e| match e {
                    RuntimeEvent::AgentActivity { label } => Some(label),
                    _ => None,
                })
                .collect();
            assert!(
                labels.iter().any(|l| l.contains(needle)),
                "advisory {kind:?} did not surface a '{needle}' activity label: {labels:?}"
            );
        }
    }

    /// The same rule for a delegation stage: the status line showed
    /// "delegation ownership_granted: 5500f56e-0773-…: src/load.js" — an
    /// internal action name and an agent UUID in the place the user looks to
    /// see what is happening.
    #[test]
    fn a_delegation_stage_reads_as_a_sentence() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(leveler_engine::EngineEvent::DelegationStage {
            action: "ownership_granted".into(),
            detail: "5500f56e-0773-4b38-8fe4-4e365c1e37db: src/load.js, test/load.test.js".into(),
        });
        let labels: Vec<String> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                RuntimeEvent::AgentActivity { label } => Some(label),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 1, "{labels:?}");
        assert!(
            !labels[0].contains("5500f56e") && !labels[0].contains("ownership_granted"),
            "no identifiers: {labels:?}"
        );
        assert!(labels[0].contains("src/load.js"), "the files: {labels:?}");
    }

    /// Stage rows are an audit trail keyed by internal identifiers. Pasting
    /// them into the status line printed "review review_launching: review" at
    /// the user — three machine words and no sentence. A required stage says
    /// what it is doing, in the words the rest of the UI uses.
    #[test]
    fn a_required_review_stage_reads_as_a_sentence() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(leveler_engine::EngineEvent::ReviewStage {
            required: true,
            action: "review_launching".into(),
            detail: "review".into(),
        });
        let labels: Vec<String> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                RuntimeEvent::AgentActivity { label } => Some(label),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 1, "{labels:?}");
        assert!(
            !labels[0].contains("review_launching") && !labels[0].contains("review review"),
            "no internal identifiers reach the user: {labels:?}"
        );
    }

    /// The direct path's structured plan must reach the client as PlanUpdated,
    /// with update_plan wire statuses mapped onto the UI step states.
    #[test]
    fn plan_updated_maps_statuses_onto_ui_plan() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);

        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::PlanUpdated {
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
            },
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
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

    /// The kernel's model-step identity rides the tool call all the way to the
    /// client. This is the fact a UI groups one model response's calls by; it
    /// is not re-derived from prose, timing or tool kind anywhere downstream.
    #[test]
    fn the_model_step_round_identity_reaches_clients() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
                parallel: false,
                model_step: Some(3),
            },
        );
        let events = drain(&mut rx);
        match events
            .iter()
            .find_map(|e| match e {
                RuntimeEvent::ToolCallStarted { id, model_step, .. } if id.as_str() == "c1" => {
                    Some(*model_step)
                }
                _ => None,
            })
            .expect("the call reached the client")
        {
            Some(3) => {}
            other => panic!("the round identity was dropped: {other:?}"),
        }
    }

    /// A command's live output and how it ended both reach clients: output
    /// as `ToolCallOutput` for that call, exit code and stop outcome on its
    /// completion.
    #[test]
    fn command_output_and_stop_outcome_reach_clients() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolCall {
                id: "c1".into(),
                name: "shell_command".into(),
                arguments: r#"{"cmd":"cargo test"}"#.into(),
                parallel: false,
                model_step: None,
            },
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolOutput {
                id: "c1".into(),
                stream: leveler_execution::OutputStream::Stderr,
                text: "Compiling leveler-core\n".into(),
            },
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolResult {
                id: "c1".into(),
                name: "shell_command".into(),
                is_error: true,
                preview: "tool error: command was cancelled".into(),
                applied_diff: None,
                exit_code: None,
                stop: Some(leveler_execution::CommandStop::Unconfirmed),
                execution_status: Some(leveler_execution::ToolExecutionStatus::CancelUnconfirmed),
            },
        );
        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                RuntimeEvent::ToolCallOutput { id, stream, chunk }
                    if id.as_str() == "c1" && stream == "stderr" && chunk == "Compiling leveler-core\n"
            )),
            "{events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                RuntimeEvent::ToolCallCompleted {
                    id,
                    exit_code: None,
                    stop: Some(leveler_client_protocol::UiCommandStop::Unconfirmed),
                    ..
                } if id.as_str() == "c1"
            )),
            "{events:?}"
        );
    }

    #[test]
    fn tool_result_pairs_by_id_even_when_out_of_order() {
        // Mimics a round mixing a parallel read (grep) with a serial edit
        // (apply_patch): the edit's result is emitted before the parallel read's.
        // A FIFO pairing would swap the two previews; id pairing keeps them right.
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolCall {
                id: "g".into(),
                name: "grep".into(),
                arguments: String::new(),
                parallel: false,
                model_step: None,
            },
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolCall {
                id: "p".into(),
                name: "apply_patch".into(),
                arguments: String::new(),
                parallel: false,
                model_step: None,
            },
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolResult {
                exit_code: None,
                stop: None,
                execution_status: None,
                id: "p".into(),
                name: "apply_patch".into(),
                is_error: false,
                preview: "AP".into(),
                applied_diff: None,
            },
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolResult {
                exit_code: None,
                stop: None,
                execution_status: None,
                id: "g".into(),
                name: "grep".into(),
                is_error: false,
                preview: "GR".into(),
                applied_diff: None,
            },
        );

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
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantDelta(" ".into()),
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::Finished(String::new()),
        );
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
        forward_agent(&mut bridge, leveler_agent::AgentEvent::StreamAttemptStarted);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantDelta("wrong".into()),
        );
        forward_agent(&mut bridge, leveler_agent::AgentEvent::StreamAttemptStarted);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantDelta("right".into()),
        );
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

    /// Live clients never see the closeout injection as a user message. The
    /// harness still emits it; the bridge only remembers it for the fold.
    #[test]
    fn a_closeout_injection_is_not_a_live_user_message() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(closeout_nudge());
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, RuntimeEvent::UserMessageAdded { .. })),
            "live closeout must not paint a user row: {events:?}"
        );
    }

    /// Display-layer fold: a later assistant message that near-duplicates an
    /// earlier one this turn (the repeated "task complete" summary after a
    /// closeout nudge) is retracted and replaced by ONE folded notice. The
    /// persisted transcript is untouched — this only stops the live UI spam.
    #[test]
    fn near_duplicate_final_summary_is_folded() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        let summary = "任务已完成:统一 closeout 决策点,合并三个 nudge 机制,四种催办原因都有 \
                       UI 事件与 transcript 持久化,工作区测试全部通过。";

        // Streamed round one passes through untouched.
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantDelta(summary.into()),
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(summary.into()),
        );
        // The harness re-drives the quiet round; THAT is what opens the fold.
        bridge.forward(closeout_nudge());
        // Nudged round two repeats the same summary with a trivial suffix.
        let repeat = format!("{summary}(以上为最终结论)");
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantDelta(repeat.clone()),
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(repeat),
        );

        let events = drain(&mut rx);
        let completed = events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::AssistantMessageCompleted { .. }))
            .count();
        assert_eq!(
            completed, 1,
            "the duplicate must not complete as a second message: {events:?}"
        );
        let started: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                RuntimeEvent::AssistantMessageStarted { message_id } => Some(message_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 2, "both rounds stream a block: {events:?}");
        assert!(
            events.iter().any(|e| matches!(
                e,
                RuntimeEvent::AssistantAttemptReset { message_id: Some(id) } if id == &started[1]
            )),
            "the duplicate's streamed block must be retracted by id: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                RuntimeEvent::Notification { message, .. } if message.contains("折叠")
            )),
            "the fold must leave one visible notice: {events:?}"
        );
    }

    /// A genuinely different second answer must never be folded.
    #[test]
    fn different_second_answer_is_not_folded() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(
                "第一部分结论:closeout 决策点已统一,三个 nudge 机制合并为共享预算。".into(),
            ),
        );
        // Even inside the nudge-opened round, a genuinely different answer is
        // not a restatement and must render.
        bridge.forward(closeout_nudge());
        forward_agent(&mut bridge, leveler_agent::AgentEvent::AssistantText(
            "补充遗漏的分支:event_bridge 的重复检测只作用于展示层,持久化与 resume 上下文都保持原样。"
                .into(),
        ));
        let events = drain(&mut rx);
        let completed = events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::AssistantMessageCompleted { .. }))
            .count();
        assert_eq!(
            completed, 2,
            "distinct answers must both render: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::Notification { message, .. } if message.contains("折叠"))),
            "no fold notice for distinct answers: {events:?}"
        );
    }

    /// Short acknowledgements repeat legitimately ("好的" twice) — the length
    /// guard keeps them out of the fold.
    #[test]
    fn short_repeats_are_not_folded() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText("好的,收到。".into()),
        );
        // Inside the nudge-opened round, so the length guard is what stops it.
        bridge.forward(closeout_nudge());
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText("好的,收到。".into()),
        );
        let events = drain(&mut rx);
        let completed = events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::AssistantMessageCompleted { .. }))
            .count();
        assert_eq!(completed, 2, "short repeats stay visible: {events:?}");
    }

    /// Regression: the fold is scoped to the round a closeout nudge opened.
    /// Ordinary progress narration that happens to resemble an earlier
    /// progress message is the model's own commentary and must survive — the
    /// harness folds repeats by lifecycle, never by how the prose reads.
    #[test]
    fn progress_repeats_are_never_folded() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        let summary = "目前确认 statusOverride 只存在于前端渲染层，后端 action 没有真正实现。";
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(summary.into()),
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolCall {
                id: "t1".into(),
                name: "grep".into(),
                arguments: "{}".into(),
                parallel: false,
                model_step: None,
            },
        );
        // A tool call followed the first text: it was progress. The nudge flag
        // was never set, so this second, near-identical text is kept.
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(
                "目前确认 statusOverride 只存在于前端渲染层，后端 action 没有真正实现。(重复)"
                    .into(),
            ),
        );
        let events = drain(&mut rx);
        let completed = events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::AssistantMessageCompleted { .. }))
            .count();
        assert_eq!(completed, 2, "both progress texts must render: {events:?}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RuntimeEvent::Notification { message, .. } if message.contains("折叠"))),
            "progress repeats are never folded: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                RuntimeEvent::AssistantAttemptReset {
                    message_id: Some(_)
                }
            )),
            "no progress block may be retracted: {events:?}"
        );
    }

    /// A nudge flag must not leak into the next turn: a round the harness never
    /// opened cannot fold anything.
    #[test]
    fn a_closeout_nudge_does_not_scope_the_next_turn() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        let summary = "这一轮的总结足够长，足以触发近重复折叠的判定阈值。";
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(summary.into()),
        );
        bridge.forward(closeout_nudge());
        bridge.forward(EngineEvent::TurnStarted {
            turn_id: leveler_core::TurnId::new("turn-2"),
            kind: leveler_engine::TurnKind::User,
        });
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(summary.into()),
        );
        let events = drain(&mut rx);
        let completed = events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::AssistantMessageCompleted { .. }))
            .count();
        assert_eq!(completed, 2, "the next turn's text renders: {events:?}");
    }

    /// The summary and its restatement the two fold-scope tests below share.
    /// They are the exact pair `near_duplicate_final_summary_is_folded` proves
    /// foldable, so "it rendered" below cannot be an accident of the prose
    /// scoring low.
    const FOLD_SCOPE_SUMMARY: &str = "任务已完成:统一 closeout 决策点,合并三个 nudge 机制,四种催办原因都有 \
                                       UI 事件与 transcript 持久化,工作区测试全部通过。";

    fn fold_scope_restatement() -> String {
        format!("{FOLD_SCOPE_SUMMARY}(以上为最终结论)")
    }

    /// Drive the lifecycle shape the pair differs by: a summary, the closeout
    /// nudge that re-drives it, then optionally a TOOL-ONLY round (tool calls,
    /// no assistant text). Returns the near-identical restatement that follows.
    fn drive_closeout_fold_scope(bridge: &mut EventBridge, tool_only_round: bool) -> String {
        forward_agent(
            bridge,
            leveler_agent::AgentEvent::AssistantText(FOLD_SCOPE_SUMMARY.into()),
        );
        bridge.forward(closeout_nudge());
        if tool_only_round {
            forward_agent(
                bridge,
                leveler_agent::AgentEvent::ToolCall {
                    id: "t1".into(),
                    name: "grep".into(),
                    arguments: "{}".into(),
                    parallel: false,
                    model_step: None,
                },
            );
            forward_agent(
                bridge,
                leveler_agent::AgentEvent::ToolResult {
                    exit_code: None,
                    stop: None,
                    execution_status: None,
                    id: "t1".into(),
                    name: "grep".into(),
                    is_error: false,
                    preview: "hit".into(),
                    applied_diff: None,
                },
            );
        }
        fold_scope_restatement()
    }

    /// Regression: the fold scope is the model round the nudge opened, not "the
    /// next assistant message after the nudge". When that round answers with
    /// tool calls and no text, the scope is over — the following round's text is
    /// the model's own progress, however closely it echoes the summary the
    /// nudge answered.
    #[test]
    fn closeout_nudge_tool_only_round_does_not_scope_following_round() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        let progress = drive_closeout_fold_scope(&mut bridge, /*tool_only_round*/ true);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(progress),
        );
        let events = drain(&mut rx);

        let completed = events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::AssistantMessageCompleted { .. }))
            .count();
        assert_eq!(
            completed, 2,
            "the following round's progress must render: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                RuntimeEvent::Notification { message, .. } if message.contains("折叠")
            )),
            "nothing may be folded after the nudge-opened round ended: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                RuntimeEvent::AssistantAttemptReset {
                    message_id: Some(_)
                }
            )),
            "no progress block may be retracted: {events:?}"
        );
    }

    /// The positive side of the same pair: when the nudge-opened round answers
    /// with the restatement itself, the fold still applies. Fixing the scope
    /// above must not disable the fold.
    #[test]
    fn direct_closeout_nudge_response_can_still_fold() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        let repeat = drive_closeout_fold_scope(&mut bridge, /*tool_only_round*/ false);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(repeat),
        );
        let events = drain(&mut rx);

        let completed = events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::AssistantMessageCompleted { .. }))
            .count();
        assert_eq!(
            completed, 1,
            "the nudge's own restatement still folds: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                RuntimeEvent::Notification { message, .. } if message.contains("折叠")
            )),
            "the fold must leave its one visible notice: {events:?}"
        );
    }

    /// The non-streamed fallback (no deltas) must fold BEFORE synthesizing the
    /// message, so the duplicate never reaches the client at all.
    #[test]
    fn non_streamed_duplicate_is_folded_without_synthesis() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        let summary = "验证完成:所有工作区测试通过,改动范围与方案一致,没有引入新的配置开关,\
                       持久化层保持不变。";
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(summary.into()),
        );
        // The fold needs the closeout nudge that re-drove the round; without it
        // the second text is ordinary narration and stays.
        bridge.forward(closeout_nudge());
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantText(summary.into()),
        );
        let events = drain(&mut rx);
        let started = events
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::AssistantMessageStarted { .. }))
            .count();
        assert_eq!(
            started, 1,
            "the duplicate must not even start a second message: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                RuntimeEvent::Notification { message, .. } if message.contains("折叠")
            )),
            "the fold must leave one visible notice: {events:?}"
        );
    }

    #[test]
    fn a_tool_call_closes_the_open_assistant_so_later_text_is_a_new_block() {
        // A round streams text, then calls a tool; the next round streams more
        // text. Without closing the assistant message at the tool call, the
        // second round's deltas reuse the first message id and the reducer
        // concatenates them into one block ("…presets fileThe test expects…").
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantDelta("round one text".into()),
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: String::new(),
                parallel: false,
                model_step: None,
            },
        );
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::AssistantDelta("round two text".into()),
        );
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
        forward_agent(
            &mut bridge,
            leveler_agent::AgentEvent::ToolResult {
                exit_code: None,
                stop: None,
                execution_status: None,
                id: "x".into(),
                name: "grep".into(),
                is_error: true,
                preview: "search budget reached".into(),
                applied_diff: None,
            },
        );
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
            model_steps: 1,
            modified_files: Vec::new(),
            stop_reason,
            stop_detail: None,
            budget_exhaustion: None,
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
    fn an_unclosed_evidence_boundary_is_a_recovery_fault_not_a_terminal() {
        let projected = turn_runtime_event(Err(AppError::UnclosedTerminalBoundary(
            "review terminal write failed".to_string(),
        )));
        assert!(matches!(
            &projected,
            RuntimeEvent::Notification {
                level: NotificationLevel::Error,
                ..
            }
        ));
        assert!(!matches!(
            &projected,
            RuntimeEvent::TurnCompleted
                | RuntimeEvent::TurnCompletedWithWarnings { .. }
                | RuntimeEvent::TurnFailed { .. }
                | RuntimeEvent::TurnCancelled
        ));
    }

    #[test]
    fn a_failed_terminal_commit_is_a_recovery_fault_not_a_terminal() {
        let projected = turn_runtime_event(Err(AppError::TerminalCommitFailed(
            "injected terminal failure".to_string(),
        )));
        assert!(matches!(
            projected,
            RuntimeEvent::Notification {
                level: NotificationLevel::Error,
                ..
            }
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

/// Projection equivalence characterization (core hardening §23). The TABLE
/// (EngineEvent inputs → client-visible shapes) is the contract: it captures
/// the behavior of the pre-hardening path and must stay green, unchanged,
/// when the projection implementation is swapped. Message/call ids are
/// normalized because the bridge mints fresh UUIDs.
#[cfg(test)]
mod projection_equivalence {
    use super::*;
    use leveler_engine::EngineEvent;

    /// Run a canonical event sequence through the application projection and
    /// return normalized shapes of every client-visible event.
    fn project(events: Vec<EngineEvent>) -> Vec<String> {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        for event in events {
            bridge.forward(event);
        }
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(shape(&ev));
        }
        out
    }

    /// Stable, id-free shape of a RuntimeEvent for table comparison.
    fn shape(ev: &RuntimeEvent) -> String {
        match ev {
            RuntimeEvent::AssistantAttemptReset { message_id } => {
                format!("reset(open={})", message_id.is_some())
            }
            RuntimeEvent::AssistantMessageStarted { .. } => "msg_start".into(),
            RuntimeEvent::AssistantTextDelta { delta, .. } => format!("delta:{delta}"),
            RuntimeEvent::AssistantMessageCompleted { .. } => "msg_done".into(),
            RuntimeEvent::ReasoningDelta { delta } => format!("reasoning:{delta}"),
            RuntimeEvent::ToolCallStarted {
                id, name, parallel, ..
            } => {
                format!("tool_start:{id}:{name}:par={parallel}")
            }
            RuntimeEvent::ToolCallCompleted { id, ok, .. } => format!("tool_done:{id}:ok={ok}"),
            RuntimeEvent::TokenUsage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
                reasoning_tokens,
            } => format!(
                "usage:{input_tokens}/{output_tokens}/{cached_input_tokens}/{}",
                reasoning_tokens.map_or("?".to_string(), |v| v.to_string())
            ),
            RuntimeEvent::Notification { level, message } => {
                format!("note[{level:?}]:{message}")
            }
            RuntimeEvent::ContextCompacted { from, to } => format!("compacted:{from}->{to}"),
            RuntimeEvent::UserShellStarted {
                execution_id,
                command,
                cwd,
            } => format!("ush_start:{}:{command}:{cwd}", execution_id.as_str()),
            RuntimeEvent::UserShellOutput {
                execution_id,
                stream,
                chunk,
            } => format!("ush_out:{}:{stream}:{chunk}", execution_id.as_str()),
            RuntimeEvent::UserShellExited {
                execution_id,
                exit_code,
                duration_ms,
                status,
            } => format!(
                "ush_exit:{}:{exit_code:?}:{duration_ms}:{status}",
                execution_id.as_str()
            ),
            RuntimeEvent::ContextExpanded {
                from_tokens,
                to_tokens,
                reason,
            } => format!("expanded:{from_tokens}->{to_tokens}:{reason}"),
            RuntimeEvent::AgentActivity { label } => format!("activity:{label}"),
            RuntimeEvent::CommandProgress { label, elapsed_ms } => {
                format!("cmd:{label}@{elapsed_ms}")
            }
            RuntimeEvent::PlanUpdated { plan } => {
                let steps: Vec<String> = plan
                    .steps
                    .iter()
                    .map(|s| format!("{}={:?}", s.index, s.status))
                    .collect();
                format!("plan:[{}]", steps.join(","))
            }
            RuntimeEvent::SubAgentUpdated { id, done, ok, .. } => {
                format!("sub:{id}:done={done}:ok={ok}")
            }
            RuntimeEvent::SubAgentProgress { id, active, .. } => {
                format!("sub_progress:{id}:active={active}")
            }
            RuntimeEvent::SubAgentActivity {
                id,
                phase,
                tool,
                is_error,
                ..
            } => {
                format!("sub_activity:{id}:{phase}:{tool}:err={is_error}")
            }
            RuntimeEvent::TurnProgress {
                phase,
                closing,
                no_progress_streak,
            } => format!("progress:{phase}:closing={closing}:streak={no_progress_streak}"),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn memory_events_project_to_structured_runtime_events() {
        let shapes = project(vec![
            EngineEvent::MemoryRecalled {
                count: 1,
                ids: vec!["k-probe".into()],
            },
            EngineEvent::MemoryChanged {
                operation: "created".into(),
                id: "k-probe".into(),
                title: "发布探针代号：ORANGE-7319".into(),
                authority: Some("explicit_user".into()),
            },
        ]);
        assert!(shapes[0].contains("MemoryRecalled"), "{shapes:?}");
        assert!(shapes[0].contains("k-probe"), "{shapes:?}");
        assert!(shapes[1].contains("MemoryChanged"), "{shapes:?}");
        assert!(shapes[1].contains("created"), "{shapes:?}");
    }

    fn tool_started(id: &str, name: &str, agent: Option<&str>) -> EngineEvent {
        EngineEvent::ToolCallStarted {
            call_id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
            parallel: false,
            risk: None,
            agent_id: agent.map(str::to_string),
            model_step: None,
        }
    }

    fn tool_finished(id: &str, name: &str, is_error: bool, agent: Option<&str>) -> EngineEvent {
        EngineEvent::ToolCallFinished {
            exit_code: None,
            stop: None,
            call_id: id.into(),
            name: name.into(),
            is_error,
            preview: "out".into(),
            agent_id: agent.map(str::to_string),
            applied_diff: None,
        }
    }

    /// A tool row's duration is how long the command ran, not how long the
    /// user took to allow it. The clock started when the MODEL asked, so a
    /// live turn reported `✓ 执行命令 · 已完成 · 3m 40s` for a script that ran
    /// for seconds and spent the rest sitting in an approval overlay — which
    /// makes every duration beside an approved command unreadable.
    #[test]
    fn an_approved_command_is_not_timed_from_before_it_was_allowed() {
        let (tx, mut rx) = broadcast::channel(64);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(tool_started("c1", "run_command", None));
        std::thread::sleep(std::time::Duration::from_millis(200));
        bridge.forward(EngineEvent::ApprovalResolved {
            id: leveler_core::ApprovalId::new("a1"),
            call_id: Some("c1".into()),
            agent_id: None,
            decision: "approve".into(),
        });
        bridge.forward(tool_finished("c1", "run_command", false, None));
        let mut seen = None;
        while let Ok(ev) = rx.try_recv() {
            if let RuntimeEvent::ToolCallCompleted { duration_ms, .. } = ev {
                seen = Some(duration_ms);
            }
        }
        let duration = seen.expect("the completed tool call reports a duration");
        assert!(
            duration < 150,
            "the 200ms spent waiting for approval was billed to the command: {duration}ms"
        );
    }

    #[test]
    fn streamed_answer_projects_start_deltas_done() {
        let shapes = project(vec![
            EngineEvent::StreamAttemptStarted,
            EngineEvent::AssistantDelta { text: "he".into() },
            EngineEvent::AssistantDelta { text: "llo".into() },
            EngineEvent::AssistantMessage {
                text: "hello".into(),
            },
        ]);
        assert_eq!(
            shapes,
            [
                "reset(open=false)",
                "msg_start",
                "delta:he",
                "delta:llo",
                "msg_done"
            ]
        );
    }

    #[test]
    fn retry_resets_the_open_message() {
        let shapes = project(vec![
            EngineEvent::AssistantDelta { text: "a".into() },
            EngineEvent::StreamAttemptStarted,
            EngineEvent::AssistantDelta { text: "b".into() },
            EngineEvent::AssistantMessage { text: "b".into() },
        ]);
        assert_eq!(
            shapes,
            [
                "msg_start",
                "delta:a",
                "reset(open=true)",
                "msg_start",
                "delta:b",
                "msg_done"
            ]
        );
    }

    #[test]
    fn non_streamed_text_synthesizes_a_whole_message() {
        assert_eq!(
            project(vec![EngineEvent::AssistantMessage { text: "hi".into() }]),
            ["msg_start", "delta:hi", "msg_done"]
        );
        assert!(project(vec![EngineEvent::AssistantMessage { text: "".into() }]).is_empty());
    }

    #[test]
    fn near_duplicate_summary_folds_to_a_notification() {
        let text = "这是一个足够长的总结内容，用来触发近重复折叠的判定逻辑。".to_string();
        let shapes = project(vec![
            EngineEvent::AssistantMessage { text: text.clone() },
            closeout_nudge(),
            EngineEvent::AssistantMessage { text },
        ]);
        assert_eq!(
            shapes[..3],
            [
                "msg_start".to_string(),
                "delta:这是一个足够长的总结内容，用来触发近重复折叠的判定逻辑。".to_string(),
                "msg_done".to_string()
            ]
        );
        assert!(shapes[3].starts_with("note[Info]"), "{shapes:?}");
        assert_eq!(shapes.len(), 4);
    }

    #[test]
    fn parent_tool_calls_project_with_duration_pairing() {
        let shapes = project(vec![
            tool_started("t1", "grep", None),
            tool_finished("t1", "grep", false, None),
        ]);
        assert_eq!(
            shapes,
            ["tool_start:t1:grep:par=false", "tool_done:t1:ok=true"]
        );
    }

    #[test]
    fn unpaired_tool_result_synthesizes_its_start() {
        let shapes = project(vec![tool_finished("t9", "apply_patch", true, None)]);
        assert_eq!(
            shapes,
            [
                "tool_start:t9:apply_patch:par=false",
                "tool_done:t9:ok=false"
            ]
        );
    }

    #[test]
    fn tool_call_closes_an_open_assistant_message() {
        let shapes = project(vec![
            EngineEvent::AssistantDelta {
                text: "think".into(),
            },
            tool_started("t1", "read_file", None),
        ]);
        assert_eq!(
            shapes,
            [
                "msg_start",
                "delta:think",
                "msg_done",
                "tool_start:t1:read_file:par=false"
            ]
        );
    }

    #[test]
    fn delegated_agent_tool_facts_do_not_render_twice() {
        // A child's canonical tool events are durable recovery facts; the UI
        // stream already carries them as attributed SubAgentActivity.
        assert!(
            project(vec![
                tool_started("c1", "grep", Some("agent-1")),
                tool_finished("c1", "grep", false, Some("agent-1")),
            ])
            .is_empty()
        );
    }

    #[test]
    fn usage_and_compaction_project() {
        let shapes = project(vec![
            EngineEvent::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 8,
                reasoning_tokens: Some(4),
            },
            EngineEvent::Compacted { from: 100, to: 40 },
        ]);
        assert_eq!(shapes[0], "usage:10/5/8/4");
        // Phase-5 structured-event migration: the compaction fact is typed;
        // clients localize ("上下文已压缩 100 → 40 条" moves to the TUI).
        assert_eq!(shapes[1], "compacted:100->40");
    }

    #[test]
    fn finalization_stages_precede_the_authoritative_terminal_event() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);

        bridge.forward(EngineEvent::AssistantDelta {
            text: "done".into(),
        });
        bridge.forward(EngineEvent::RunFinished {
            text: "done".into(),
        });
        bridge.forward(EngineEvent::FinalizationStarted {
            at: leveler_core::now(),
        });
        bridge.forward(EngineEvent::FinalizationPhaseStarted {
            phase: "review".into(),
            at: leveler_core::now(),
        });
        bridge.forward(EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Completed,
            reason: None,
            failure: None,
            stop: Some(leveler_agent::StopReason::Completed),
            warnings: Vec::new(),
        });

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEvent::AssistantMessageStarted { .. },
                RuntimeEvent::AssistantTextDelta { .. },
                RuntimeEvent::AssistantMessageCompleted { .. },
                RuntimeEvent::TurnFinalizing {
                    stage: FinalizationStage::SettlingDependencies
                },
                RuntimeEvent::TurnFinalizing {
                    stage: FinalizationStage::Review
                },
                RuntimeEvent::TurnCompleted
            ]
        ));
        assert!(bridge.terminal_published());
    }

    #[test]
    fn terminal_is_enqueued_before_admission_is_released() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (tx, _rx) = broadcast::channel(4);
        let mut callback_rx = tx.subscribe();
        let terminal_was_visible = Arc::new(AtomicBool::new(false));
        let observed = terminal_was_visible.clone();
        let mut bridge = EventBridge::new(tx.clone()).with_terminal_publisher(move |event| {
            let _ = tx.send(event);
            observed.store(
                matches!(callback_rx.try_recv(), Ok(RuntimeEvent::TurnCompleted)),
                Ordering::SeqCst,
            );
        });

        bridge.forward(EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Completed,
            reason: None,
            failure: None,
            stop: Some(leveler_agent::StopReason::Completed),
            warnings: Vec::new(),
        });

        assert!(terminal_was_visible.load(Ordering::SeqCst));
    }

    #[test]
    fn completion_warning_is_preserved() {
        let (tx, mut rx) = broadcast::channel(4);
        let mut bridge = EventBridge::new(tx);

        bridge.forward(EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Completed,
            reason: Some("required independent review did not complete".into()),
            failure: None,
            stop: Some(leveler_agent::StopReason::Completed),
            warnings: vec!["required independent review did not complete".into()],
        });

        assert!(matches!(
            rx.try_recv(),
            Ok(RuntimeEvent::TurnCompletedWithWarnings { reason })
                if reason == "required independent review did not complete"
        ));
    }

    #[test]
    fn legacy_completed_task_without_typed_stop_stays_completed() {
        let (tx, mut rx) = broadcast::channel(4);
        let mut bridge = EventBridge::new(tx);

        bridge.forward(EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Completed,
            reason: None,
            failure: None,
            stop: None,
            warnings: Vec::new(),
        });

        assert!(matches!(rx.try_recv(), Ok(RuntimeEvent::TurnCompleted)));
    }

    #[test]
    fn terminal_latch_drops_every_post_terminal_event() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Completed,
            reason: None,
            failure: None,
            stop: Some(leveler_agent::StopReason::Completed),
            warnings: Vec::new(),
        });
        assert!(matches!(rx.try_recv(), Ok(RuntimeEvent::TurnCompleted)));

        bridge.forward(EngineEvent::FinalizationPhaseStarted {
            phase: "cleanup".into(),
            at: leveler_core::now(),
        });
        bridge.forward(EngineEvent::AssistantMessage {
            text: "post-terminal review".into(),
        });
        bridge.forward(EngineEvent::AssistantDelta {
            text: "ignored".into(),
        });
        bridge.forward(EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Failed,
            reason: Some("duplicate".into()),
            failure: None,
            stop: None,
            warnings: Vec::new(),
        });

        assert!(
            rx.try_recv().is_err(),
            "nothing may make a terminal UI busy or publish a second terminal"
        );

        bridge.forward(EngineEvent::GoalCheckpointCreated {
            checkpoint_id: "checkpoint-1".into(),
            goal_id: "goal-1".into(),
            reason: "milestone".into(),
            created_at: "2026-09-14T00:00:00Z".into(),
            payload: Box::new(leveler_lifecycle::GoalCheckpoint {
                objective: "continue the goal".into(),
                ..Default::default()
            }),
        });
        assert!(rx.try_recv().is_err());
    }

    /// The projection is what every client renders. Facts the runtime already
    /// computed must survive the hop: a contribution dropped here cannot be
    /// recovered downstream, and the UI would have to invent it or omit it.
    #[test]
    fn a_child_contribution_survives_the_bridge() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(EngineEvent::SubAgentFinished {
            id: "a1".into(),
            nickname: "Newton".into(),
            ok: true,
            summary: "done".into(),
            contribution: Some(
                leveler_lifecycle::ChildResultProjection::from_findings("a1", "explorer", &[])
                    .with_profile("explorer", "explorer", true),
            ),
            outcome: None,
            stop: None,
            limit: None,
        });
        let ev = rx.try_recv().expect("one event");
        match ev {
            RuntimeEvent::SubAgentUpdated {
                role, contribution, ..
            } => {
                assert_eq!(role, "explorer", "role must not be blanked on finish");
                let c = contribution.expect("the projection must reach the client");
                assert_eq!(c.role, "explorer");
                assert_eq!(c.profile_id.as_deref(), Some("explorer"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// The typed terminal crosses the bridge as typed fields. A client that
    /// has to recover "partial" or "lost" from the summary text is deriving
    /// runtime truth, which is exactly what it must not do.
    #[test]
    fn a_child_typed_terminal_survives_the_bridge() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(EngineEvent::SubAgentFinished {
            id: "a1".into(),
            nickname: "Newton".into(),
            ok: false,
            summary: "stopped".into(),
            contribution: None,
            outcome: Some(leveler_lifecycle::ChildStatus::IncompletePartial),
            stop: Some(leveler_lifecycle::ChildStop::Budget),
            limit: None,
        });
        match rx.try_recv().expect("one event") {
            RuntimeEvent::SubAgentUpdated { outcome, stop, .. } => {
                assert_eq!(
                    outcome,
                    Some(leveler_client_protocol::ChildOutcome::IncompletePartial)
                );
                assert_eq!(stop, Some(leveler_client_protocol::ChildStop::Budget));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// The bound behind a budget stop crosses the bridge typed. Without it a
    /// client cannot tell a wall-clock timeout from a spent token budget, and
    /// every budget stop has to read the same generic word.
    #[test]
    fn a_child_budget_limit_survives_the_bridge() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(EngineEvent::SubAgentFinished {
            id: "a1".into(),
            nickname: "Euclid".into(),
            ok: false,
            summary: "stopped".into(),
            contribution: None,
            outcome: Some(leveler_lifecycle::ChildStatus::IncompletePartial),
            stop: Some(leveler_lifecycle::ChildStop::Budget),
            limit: Some(leveler_lifecycle::ChildLimit::Duration),
        });
        match rx.try_recv().expect("one event") {
            RuntimeEvent::SubAgentUpdated {
                outcome,
                stop,
                limit,
                ..
            } => {
                assert_eq!(
                    outcome,
                    Some(leveler_client_protocol::ChildOutcome::IncompletePartial)
                );
                assert_eq!(stop, Some(leveler_client_protocol::ChildStop::Budget));
                assert_eq!(
                    limit,
                    Some(leveler_client_protocol::ChildLimit::Duration),
                    "the bound must not be dropped at the bridge"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// A child spawned from a declarative agent carries the agent it was
    /// resolved from — name, source, fingerprint, model, effort, skills — so a
    /// client can say "Curie · security-reviewer", not just "explorer".
    #[test]
    fn a_child_start_carries_its_agent_identity() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(EngineEvent::SubAgentStarted {
            id: "a1".into(),
            nickname: "Curie".into(),
            role: "explorer".into(),
            task: "review".into(),
            profile_id: Some("security-reviewer".into()),
            profile_role: Some("explorer".into()),
            read_only: true,
            spec: Some(leveler_lifecycle::ChildSpawnSpec {
                model: Some("deepseek/deepseek-v4-pro".into()),
                agent: Some(Box::new(leveler_lifecycle::ChildAgentSnapshot {
                    name: "security-reviewer".into(),
                    source: "project".into(),
                    fingerprint: "sha256:abc".into(),
                    capability: "read_only".into(),
                    reasoning_effort: Some("high".into()),
                    skills: vec!["sec-audit".into()],
                    ..Default::default()
                })),
                ..Default::default()
            }),
        });
        match rx.try_recv().expect("start") {
            RuntimeEvent::SubAgentUpdated { agent, .. } => {
                let agent = agent.expect("identity carried");
                assert_eq!(agent.name, "security-reviewer");
                assert_eq!(agent.source, "project");
                assert_eq!(agent.fingerprint, "sha256:abc");
                assert_eq!(agent.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
                assert_eq!(agent.reasoning_effort.as_deref(), Some("high"));
                assert_eq!(agent.skills, vec!["sec-audit".to_string()]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// A child's background flag and scope reach the client at start, and an
    /// interruption or resume reaches it as a state change — a client must not
    /// keep drawing a dead activation as running.
    #[test]
    fn a_child_start_and_its_lifecycle_moves_reach_the_client() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(EngineEvent::SubAgentStarted {
            id: "a1".into(),
            nickname: "Newton".into(),
            role: "worker".into(),
            task: "write".into(),
            profile_id: None,
            profile_role: None,
            read_only: false,
            spec: Some(leveler_lifecycle::ChildSpawnSpec {
                files: vec!["src/a.rs".into()],
                background: true,
                ..Default::default()
            }),
        });
        match rx.try_recv().expect("start") {
            RuntimeEvent::SubAgentUpdated {
                background, scope, ..
            } => {
                assert_eq!(background, Some(true));
                assert_eq!(scope, vec!["src/a.rs".to_string()]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        bridge.forward(EngineEvent::SubAgentInterrupted { id: "a1".into() });
        bridge.forward(EngineEvent::SubAgentResumed {
            id: "a1".into(),
            attempt: 1,
        });
        let states: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| match event {
                RuntimeEvent::SubAgentStateChanged { id, state } => (id, state),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(
            states,
            vec![
                (
                    "a1".to_string(),
                    leveler_client_protocol::UiChildState::Interrupted
                ),
                (
                    "a1".to_string(),
                    leveler_client_protocol::UiChildState::Running
                ),
            ]
        );
    }

    /// A terminal says nothing about whether the child ran in the background;
    /// it must not claim `false`.
    #[test]
    fn a_child_terminal_does_not_claim_a_background_flag() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(EngineEvent::SubAgentFinished {
            id: "a1".into(),
            nickname: "Newton".into(),
            ok: true,
            summary: "done".into(),
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
        });
        match rx.try_recv().expect("one event") {
            RuntimeEvent::SubAgentUpdated { background, .. } => assert_eq!(background, None),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// `None` means the runtime did not measure this child. It must stay
    /// distinguishable from a measured zero all the way to the renderer.
    #[test]
    fn an_unmeasured_contribution_stays_none() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(EngineEvent::SubAgentFinished {
            id: "a1".into(),
            nickname: "Newton".into(),
            ok: false,
            summary: "did not report".into(),
            contribution: None,
            outcome: None,
            stop: None,
            limit: None,
        });
        match rx.try_recv().expect("one event") {
            RuntimeEvent::SubAgentUpdated { contribution, .. } => {
                assert!(contribution.is_none(), "unmeasured must not become a zero");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// The capability contract is what lets the UI say "read-only" instead of
    /// implying it.
    #[test]
    fn a_child_profile_survives_the_bridge() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut bridge = EventBridge::new(tx);
        bridge.forward(EngineEvent::SubAgentStarted {
            id: "r1".into(),
            nickname: "reviewer".into(),
            role: "reviewer".into(),
            task: "review the diff".into(),
            profile_id: Some("reviewer".into()),
            profile_role: Some("reviewer".into()),
            read_only: true,
            spec: None,
        });
        match rx.try_recv().expect("one event") {
            RuntimeEvent::SubAgentUpdated {
                profile_id,
                read_only,
                ..
            } => {
                assert_eq!(profile_id.as_deref(), Some("reviewer"));
                assert!(read_only, "a reviewer's write bound must cross the wire");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn sub_agent_lifecycle_projects() {
        let shapes = project(vec![
            EngineEvent::SubAgentStarted {
                id: "a1".into(),
                nickname: "Newton".into(),
                role: "explorer".into(),
                task: "look".into(),
                profile_id: None,
                profile_role: None,
                read_only: false,
                spec: None,
            },
            EngineEvent::SubAgentProgress {
                id: "a1".into(),
                active: true,
                input_tokens: 1,
                output_tokens: 2,
                cached_input_tokens: 0,
            },
            EngineEvent::SubAgentActivity {
                id: "a1".into(),
                phase: "tool_started".into(),
                tool: "grep".into(),
                preview: String::new(),
                is_error: false,
            },
            EngineEvent::SubAgentFinished {
                id: "a1".into(),
                nickname: "Newton".into(),
                ok: true,
                summary: "done".into(),
                contribution: None,
                outcome: None,
                stop: None,
                limit: None,
            },
        ]);
        assert_eq!(
            shapes,
            [
                "sub:a1:done=false:ok=false",
                "sub_progress:a1:active=true",
                "sub_activity:a1:tool_started:grep:err=false",
                "sub:a1:done=true:ok=true"
            ]
        );
    }

    #[test]
    fn advisory_and_command_progress_project() {
        let shapes = project(vec![
            EngineEvent::AdvisoryStarted {
                kind: "context_compaction".into(),
            },
            EngineEvent::CommandProgress {
                label: "cargo test".into(),
                elapsed_ms: 1500,
            },
        ]);
        assert!(shapes[0].starts_with("activity:"), "{shapes:?}");
        assert_eq!(shapes[1], "cmd:cargo test@1500");
    }

    #[test]
    fn goal_interception_surfaces_as_activity() {
        let shapes = project(vec![EngineEvent::GoalIntercepted {
            kind: "complete".into(),
            detail: "checks failing".into(),
        }]);
        assert_eq!(shapes, ["activity:gate refused complete: checks failing"]);
    }

    #[test]
    fn finished_closes_a_dangling_message() {
        let shapes = project(vec![
            EngineEvent::AssistantDelta {
                text: "tail".into(),
            },
            EngineEvent::RunFinished {
                text: "tail".into(),
            },
        ]);
        assert_eq!(shapes, ["msg_start", "delta:tail", "msg_done"]);
    }

    #[test]
    fn engine_only_facts_do_not_reach_the_client_stream() {
        // Lifecycle, approvals, and strategy events are surfaced by
        // engine-aware consumers (snapshot, approval channel, eval) — the
        // client event stream must not see them.
        let shapes = project(vec![
            EngineEvent::TurnStarted {
                turn_id: leveler_core::TurnId::new("turn-1"),
                kind: leveler_engine::TurnKind::Chat,
            },
            EngineEvent::ContextSnapshot {
                messages: Vec::new(),
                through_ordinal: None,
            },
            EngineEvent::PhaseChanged {
                from: leveler_lifecycle::AgentState::Plan,
                to: leveler_lifecycle::AgentState::Execute,
            },
        ]);
        assert!(shapes.is_empty(), "{shapes:?}");
    }

    #[test]
    fn user_shell_lifecycle_projects_one_to_one() {
        let id = leveler_core::UserShellId::new("ush-1");
        let shapes = project(vec![
            EngineEvent::UserShellStarted {
                execution_id: id.clone(),
                command: "cargo test".into(),
                cwd: "/repo".into(),
            },
            EngineEvent::UserShellOutput {
                execution_id: id.clone(),
                stream: "stdout".into(),
                chunk: "running".into(),
            },
            EngineEvent::UserShellFinished {
                execution_id: id,
                exit_code: Some(0),
                duration_ms: 4200,
                status: "success".into(),
            },
        ]);
        assert_eq!(
            shapes,
            [
                "ush_start:ush-1:cargo test:/repo",
                "ush_out:ush-1:stdout:running",
                "ush_exit:ush-1:Some(0):4200:success"
            ]
        );
    }

    #[test]
    fn context_expansion_now_reaches_the_client() {
        // DELIBERATE behavior change, the one exception in this table: the
        // legacy shim dropped this durable fact before the bridge's
        // notification arm could run (dead code confirmed in the audit). The
        // canonical projection restores the intended notification. Production
        // default keeps adaptive context disabled, so nothing fires today.
        let shapes = project(vec![EngineEvent::ContextExpanded {
            from: 256_000,
            to: 512_000,
            reason: "reread_pressure".into(),
            crossed_reliable: false,
        }]);
        assert_eq!(shapes.len(), 1, "{shapes:?}");
        assert_eq!(shapes[0], "expanded:256000->512000:reread_pressure");
    }
}
