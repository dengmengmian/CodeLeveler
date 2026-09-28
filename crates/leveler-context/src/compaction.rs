//! Anchored context compaction: span selection, folding, token estimate.

use leveler_model::{ContentPart, Message, Role, RuntimeNoticeKind, TranscriptOrigin};

/// How many trailing messages (the working set) auto-compaction keeps verbatim.
pub const COMPACT_KEEP_RECENT: usize = 12;

/// Default token estimate threshold for host-side pre-request compact (engine chat).
/// Matches a conservative mid-size window so long histories fold before the API call.
pub const PRE_REQUEST_COMPACT_THRESHOLD: u64 = 24_000;

/// The instruction that asks the model to write a handoff briefing for the
/// rounds compaction is about to elide. A bare "N steps dropped" breadcrumb
/// throws away every decision, dead end, and finding from those rounds, so the
/// resumed model redoes work it already did; this keeps the reasoning, not just
/// the file names.
pub(crate) const COMPACT_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. \
     The earlier messages above are about to be dropped from your working context. \
     Write a handoff briefing for the model that resumes this task.\n\
     Include:\n\
     - Progress so far and the key decisions made, with the reasoning behind them\n\
     - What you learned about the codebase: the real paths, symbols, and their roles\n\
     - Approaches already tried that FAILED, so they are not attempted again\n\
     - Constraints, requirements, and user preferences stated so far\n\
     - What remains to be done, as concrete next steps\n\
     Be specific and cite real paths. Reply with ONLY the briefing.";

/// Stable prefix of the fold breadcrumb (see `compact_messages`). Used both to
/// build the breadcrumb and to detect, on a later fold, that an earlier briefing
/// already exists so the summarizer can UPDATE it instead of re-deriving it.
/// The authoritative definition lives in `leveler-model` because the breadcrumb
/// is model-visible content and context accounting detects it there.
pub(crate) use leveler_model::COMPACTION_BREADCRUMB_MARKER;

/// The instruction used when the range being summarized already contains a prior
/// fold briefing. Re-summarizing a summary from scratch loses fidelity a little
/// more each fold; this tells the model to carry the earlier briefing's still-
/// relevant facts forward verbatim and only fold in what happened since.
pub(crate) const COMPACT_UPDATE_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. \
     An EARLIER handoff briefing already appears in the messages above (it starts with \
     \"[Earlier context was compacted\"). Produce an UPDATED briefing that MERGES that earlier \
     briefing with the newer work about to be dropped.\n\
     Rules:\n\
     - Preserve every still-relevant fact, decision, failed approach, real path, and constraint \
     from the earlier briefing — do not drop them just because they are older.\n\
     - Fold in what happened since: new progress, decisions, findings, and failed approaches.\n\
     - Drop only what later work has made obsolete or superseded, and say what replaced it.\n\
     Keep the same sections (progress, learnings, failed approaches, constraints, next steps). \
     Be specific and cite real paths. Reply with ONLY the updated briefing.";

/// The first index at or before `start` where a transcript tail may legally
/// begin.
///
/// A tool exchange — an assistant `tool_calls` message and the tool message(s)
/// answering it — is one unit: every provider rejects a `role: tool` message
/// whose `tool_calls` are not directly before it. A tail bounded by message
/// count or token budget can land inside an exchange, so every slice of a
/// transcript passes through here and moves back over the exchange's results
/// to the assistant that opened it.
///
/// It never moves forward past a result. A leading result the input itself
/// cannot pair is left in place for the provider boundary to refuse; dropping
/// it here would hide whatever built that input wrong.
pub fn round_boundary(messages: &[Message], start: usize) -> usize {
    let mut start = start.min(messages.len());
    while start > 0 && start < messages.len() && messages[start].role == Role::Tool {
        start -= 1;
    }
    start
}

/// The head/middle/tail split for compaction: `(head_end, tail_start)`, or None
/// when there is nothing worth folding. Cuts only at round boundaries so a
/// tool-call is never separated from its tool-result (the provider rejects
/// orphaned tool calls).
///
/// `keep_recent` bounds the working set by MESSAGE COUNT; `keep_recent_tokens`
/// (0 = disabled) additionally bounds it by an estimated TOKEN budget. A fixed
/// count is fragile: a single huge tool output inside the last `keep_recent`
/// messages keeps the folded transcript over the window and defeats the fold.
/// The token cap can only *shrink* the retained tail (drop older-of-recent into
/// the summarized middle), never grow it, so count-based behavior is unchanged
/// whenever the recent window fits the budget.
pub(crate) fn compaction_span(
    messages: &[Message],
    keep_recent: usize,
    keep_recent_tokens: u64,
) -> Option<(usize, usize)> {
    // Head: the system prompt(s) plus the first user message (the task anchor).
    let head_end = messages
        .iter()
        .position(|m| m.role == Role::User)
        .map(|i| i + 1)
        .unwrap_or(0);

    // Tail start: keep the last `keep_recent` messages…
    let mut tail_start = messages.len().saturating_sub(keep_recent).max(head_end);

    // …but if that working set blows the token budget, walk the start forward
    // (drop the oldest recent messages into the summarized middle) until it fits.
    // Always keep at least the newest message — it is usually what just overflowed.
    if keep_recent_tokens > 0 {
        while tail_start < messages.len().saturating_sub(1)
            && estimate_tokens(&messages[tail_start..]) > keep_recent_tokens
        {
            tail_start += 1;
        }
    }

    // Never begin the tail on a Tool result — back up to its owning assistant so
    // the pair stays whole (the provider rejects orphaned tool results).
    tail_start = round_boundary(messages, tail_start).max(head_end);

    // Nothing meaningful in the middle → leave it alone.
    if tail_start <= head_end || tail_start - head_end < 2 {
        return None;
    }
    Some((head_end, tail_start))
}

/// What a measured request's pressure means for the context fold, and
/// therefore what a failed briefing costs the turn.
///
/// The fold has TWO bounds, and they answer different questions. The quality
/// threshold is where recall is expected to degrade — folding there is a
/// quality choice that may be abandoned. The hard capacity is where a request
/// can no longer legally be sent — folding there is REQUIRED, and a fold that
/// cannot be produced must still be brought under capacity mechanically or the
/// turn fails explicitly.
///
/// This is the ONE contract every active-context entry shares: coding/drive,
/// chat, resume and [`crate`]-level assembly. Entries provide the measured
/// facts (projected tokens, resolved bounds); this decides what a failure
/// means. A caller must never re-derive the split from a model identity, a
/// turn count, or an "is it stuck" guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldRequirement {
    /// At or below the quality boundary: nothing to fold.
    None,
    /// Over the quality boundary but within hard capacity. Sending the request
    /// uncompacted is legal, so a failed briefing keeps the original active
    /// history and the turn continues.
    Soft,
    /// Over the hard capacity. Without a fold the request cannot legally be
    /// sent, so a failed briefing must be folded mechanically or the turn
    /// fails without ever sending an oversized request.
    HardRequired,
}

impl FoldRequirement {
    /// Classify from facts only.
    ///
    /// `hard_capacity` is `None` when the model declares no window: with no
    /// hard limit there is no request compaction is obliged to make legal, so
    /// nothing may claim a hard requirement.
    pub fn classify(
        projected_tokens: u64,
        quality_threshold: u64,
        hard_capacity: Option<u64>,
    ) -> Self {
        if let Some(capacity) = hard_capacity
            && projected_tokens > capacity
        {
            return Self::HardRequired;
        }
        if projected_tokens > quality_threshold {
            return Self::Soft;
        }
        Self::None
    }

    /// Whether a fold must succeed (mechanically at minimum) before the next
    /// request can be sent.
    pub fn fold_is_mandatory(self) -> bool {
        matches!(self, Self::HardRequired)
    }
}

/// Marker for the host-pinned active objective re-injected after compaction.
/// Marks the host-pinned objective block so a fold can recognise and
/// replace its own previous injection instead of stacking a second one.
pub const ACTIVE_OBJECTIVE_MARKER: &str = "[Active objective — host-pinned]";

/// Build the user message that re-pins the host objective after a fold.
pub(crate) fn objective_pin_message(objective: &str) -> Message {
    let obj = objective.trim();
    Message {
        origin: Some(TranscriptOrigin::RuntimeNotice {
            notice: RuntimeNoticeKind::ObjectivePin,
        }),
        role: Role::User,
        content: vec![ContentPart::Text {
            text: format!(
                "{ACTIVE_OBJECTIVE_MARKER}\n\
                 <objective>\n{obj}\n</objective>\n\
                 This is the active request for this turn."
            ),
        }],
    }
}

/// Whether the transcript already carries a host pin for this objective text.
pub(crate) fn transcript_has_objective_pin(messages: &[Message], objective: &str) -> bool {
    let obj = objective.trim();
    if obj.is_empty() {
        return true;
    }
    messages.iter().any(|m| {
        m.role == Role::User
            && m.text_content().contains(ACTIVE_OBJECTIVE_MARKER)
            && m.text_content().contains(obj)
    })
}

/// Anchored compaction (spec §53): fold a long in-memory transcript back under
/// the context window. Keeps the system prompt + first user (history head) and
/// the last `keep_recent` messages (the working set), and replaces the elided
/// middle with `summary` — the model-written handoff briefing.
///
/// When `active_objective` is set, a host-pinned `<objective>` user message is
/// always re-injected after the head so multi-turn Chat does not keep only the
/// *first* user line as the task (ObjectiveAnchor is the SoT).
///
/// `summary` is None only when the summarization call was unavailable; the fold
/// then degrades to a bare breadcrumb, which says so explicitly rather than
/// pretending the history was preserved. Returns the input unchanged when there
/// is nothing worth compacting (still may inject an objective pin if missing).
pub fn compact_messages(
    messages: &[Message],
    keep_recent: usize,
    keep_recent_tokens: u64,
    summary: Option<&str>,
    active_objective: Option<&str>,
) -> Vec<Message> {
    let pin = active_objective
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(objective_pin_message);

    let Some((head_end, tail_start)) = compaction_span(messages, keep_recent, keep_recent_tokens)
    else {
        // Nothing to fold — still ensure the host objective is present.
        if let Some(pin) = pin
            && !transcript_has_objective_pin(messages, active_objective.unwrap_or(""))
        {
            let mut out = messages.to_vec();
            // After leading system messages.
            let insert_at = out
                .iter()
                .position(|m| m.role != Role::System)
                .unwrap_or(out.len());
            out.insert(insert_at, pin);
            return out;
        }
        return messages.to_vec();
    };
    let middle = &messages[head_end..tail_start];

    // Scoped project rules arrive as mid-transcript system messages. They are
    // standing constraints, not elidable history — carry them across the fold.
    let carried: Vec<Message> = middle
        .iter()
        .filter(|m| m.role == Role::System)
        .cloned()
        .collect();
    let elided = middle.len() - carried.len();

    // Files the elided steps touched, gathered from tool-call args. Cheap, exact,
    // and useful even when the summary is present.
    let mut files: Vec<String> = Vec::new();
    for m in middle {
        for part in &m.content {
            if let ContentPart::ToolCall { call } = part
                && let Some(p) = call.arguments.get("path").and_then(|v| v.as_str())
                && !files.contains(&p.to_string())
            {
                files.push(p.to_string());
            }
        }
    }
    files.truncate(20);
    let files_note = if files.is_empty() {
        String::new()
    } else {
        format!(" Files touched: {}.", files.join(", "))
    };
    let body = match summary {
        Some(summary) => format!(
            "[Earlier context was compacted to fit the window: {elided} steps elided.{files_note}]\n\n\
             Earlier conversation history was compacted. {elided} steps were elided.\n\
             The following summary was generated from the elided history and may omit details:\n\
             {summary}\n"
        ),
        None => format!(
            "[Earlier context was compacted to fit the window: {elided} steps elided.{files_note} \
             Summarization was unavailable. Some earlier details are no longer available \
             in the active context.]"
        ),
    };
    let breadcrumb = Message {
        origin: Some(TranscriptOrigin::CompactionSummary),
        role: Role::User,
        content: vec![ContentPart::Text { text: body }],
    };

    let mut out =
        Vec::with_capacity(head_end + 1 + carried.len() + 1 + (messages.len() - tail_start));
    out.extend_from_slice(&messages[..head_end]);
    // Host objective always re-pinned after fold (even if first User differs).
    if let Some(pin) = pin {
        out.push(pin);
    }
    out.extend(carried);
    out.push(breadcrumb);
    out.extend_from_slice(&messages[tail_start..]);
    out
}

/// Which briefing instruction applies to the range about to be summarized: if it
/// already contains an earlier fold breadcrumb, UPDATE that briefing rather than
/// re-summarizing a summary from scratch (repeated from-scratch folds lose the
/// oldest facts a little more each time).
pub(crate) fn summary_prompt_for(to_summarize: &[Message]) -> &'static str {
    let has_prior_briefing = to_summarize
        .iter()
        .any(|m| m.text_content().contains(COMPACTION_BREADCRUMB_MARKER));
    if has_prior_briefing {
        COMPACT_UPDATE_PROMPT
    } else {
        COMPACT_PROMPT
    }
}

/// Mechanical acceptance for a summary that will replace model-visible history.
pub fn accepted_summary(response: &leveler_model::ModelResponse) -> Option<String> {
    if response.finish_reason != leveler_model::FinishReason::Stop
        || response
            .message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::ToolCall { .. }))
    {
        return None;
    }
    let text = response.message.text_content().trim().to_string();
    (!text.is_empty()).then_some(text)
}

pub async fn summary_request(
    runtime: &dyn leveler_model::ModelRuntime,
    model: &leveler_model::ModelRef,
    reasoning_effort: Option<leveler_model::ReasoningEffort>,
    messages: &[Message],
    keep_recent: usize,
    keep_recent_tokens: u64,
    resolved_max_output_tokens: u32,
) -> Option<leveler_model::ModelRequest> {
    let (_, tail_start) = compaction_span(messages, keep_recent, keep_recent_tokens)?;
    let to_summarize = &messages[..tail_start];
    let mut summary_messages = to_summarize.to_vec();
    summary_messages.push(Message::text(Role::User, summary_prompt_for(to_summarize)));

    let profile = runtime.profile(model).await.ok()?;
    let reasoning_effort =
        leveler_model::resolve_reasoning_effort(reasoning_effort, &profile.reasoning).effective;
    let mut request = leveler_model::ModelRequest::new(model.clone(), summary_messages);
    request.tool_choice = leveler_model::ToolChoice::None;
    // Summaries use the seat's resolved completion allowance, bounded by
    // the provider declaration. Reasoning and the final briefing share this
    // allowance; a fixed 1024 cap can leave no room for either to complete.
    let output_cap = resolved_max_output_tokens.min(profile.limits.max_output_tokens);
    if output_cap == 0
        || matches!(profile.reasoning.style,
            leveler_model::ReasoningStyle::BudgetedThinking { budget_tokens }
                if budget_tokens >= output_cap)
    {
        // No valid summary can fit. The caller keeps its original context.
        return None;
    }
    request.max_output_tokens = Some(output_cap);
    request.reasoning_effort = reasoning_effort;
    request.projection = Some(leveler_model::RequestProjection::project(
        &request.messages,
        &request.tools,
        leveler_model::ReasoningReplayContract::resolve(profile.protocol, &profile.compatibility),
        leveler_model::ReasoningRetention::All,
    ));

    Some(request)
}

#[cfg(test)]
mod summary_acceptance_tests {
    use super::accepted_summary;
    use leveler_model::{
        ContentPart, FinishReason, Message, ModelResponse, Role, TokenUsage, ToolCall,
    };

    #[test]
    fn only_a_complete_nonempty_tool_free_summary_is_accepted() {
        let mut response = ModelResponse {
            request_id: leveler_core::RequestId::generate(),
            message: Message::text(Role::Assistant, " summary "),
            usage: TokenUsage::default(),
            finish_reason: FinishReason::Stop,
        };
        assert_eq!(accepted_summary(&response).as_deref(), Some("summary"));
        for reason in [
            FinishReason::Length,
            FinishReason::ContentFilter,
            FinishReason::Other,
            FinishReason::ToolCalls,
        ] {
            response.finish_reason = reason;
            assert_eq!(accepted_summary(&response), None, "{reason:?}");
        }
        response.finish_reason = FinishReason::Stop;
        response.message.content.push(ContentPart::ToolCall {
            call: ToolCall {
                id: leveler_core::ToolCallId::new("unexpected"),
                name: "read_file".into(),
                arguments: Default::default(),
            },
        });
        assert_eq!(accepted_summary(&response), None);
        response.message = Message::text(Role::Assistant, " \n ");
        assert_eq!(accepted_summary(&response), None);
        response.message = Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::Reasoning {
                text: "unfinished internal reasoning".into(),
            }],
        };
        assert_eq!(
            accepted_summary(&response),
            None,
            "reasoning is never promoted into a briefing"
        );
    }
}

/// Coarse token estimate over a transcript's model-visible content.
///
/// The implementation — and the single definition of the density weights —
/// lives in `leveler-model` (`leveler_model::estimate`), because the same
/// arithmetic must serve compaction pressure, the agent's token-budget
/// fallback and the context-accounting breakdown. Re-exported here so the
/// context crate's callers keep one import.
pub use leveler_model::estimate_tokens;

#[cfg(test)]
mod estimate_tests {
    use super::*;
    use leveler_model::Role;

    #[test]
    fn tool_payloads_are_weighted_denser_than_prose() {
        // Measured against DeepSeek-reported usage (C5-S2): tool payloads
        // tokenize at ~2.5-2.9 bytes/token vs prose's ~4. The same bytes as
        // a tool result must therefore estimate higher than as plain text.
        let body = "{\"path\":\"src/lib.rs\",\"exit\":0}".repeat(100);
        let as_text = estimate_tokens(&[Message::text(Role::User, body.clone())]);
        let as_tool = estimate_tokens(&[Message {
            origin: None,
            role: Role::User,
            content: vec![ContentPart::ToolResult {
                result: leveler_model::ToolResultContent {
                    call_id: leveler_core::ToolCallId::new("c"),
                    content: body,
                    is_error: false,
                },
            }],
        }]);
        assert!(
            as_tool > as_text * 3 / 2,
            "tool weighting missing: text={as_text} tool={as_tool}"
        );
    }

    /// Frozen calibration regressions (C5-S2). Each fixture is the exact byte
    /// sequence measured against DeepSeek `prompt_tokens` on 2026-08-10
    /// (framing-corrected); the tolerance encodes the acceptance gate: never
    /// under-estimate by more than 10%, over-estimation bounded at 40%.
    #[test]
    fn calibration_fixtures_stay_within_measured_tolerance() {
        for (fixture, actual, tool) in [
            (
                include_str!("../tests/fixtures/calib-rust-small.txt"),
                2064u64,
                false,
            ),
            (
                include_str!("../tests/fixtures/calib-json-small.txt"),
                2763,
                true,
            ),
            (
                include_str!("../tests/fixtures/calib-tool-small.txt"),
                3105,
                true,
            ),
        ] {
            let message = if tool {
                Message {
                    origin: None,
                    role: Role::User,
                    content: vec![ContentPart::ToolResult {
                        result: leveler_model::ToolResultContent {
                            call_id: leveler_core::ToolCallId::new("c"),
                            content: fixture.to_string(),
                            is_error: false,
                        },
                    }],
                }
            } else {
                Message::text(Role::User, fixture.to_string())
            };
            let est = estimate_tokens(&[message]);
            assert!(
                est * 10 >= actual * 9,
                "under-estimates measured ground truth by >10%: est={est} actual={actual}"
            );
            assert!(
                est * 10 <= actual * 14,
                "over-estimates measured ground truth by >40%: est={est} actual={actual}"
            );
        }
    }

    #[test]
    fn cjk_text_is_not_underestimated() {
        // Common tokenizers spend ~1 token per CJK char. Plain bytes/4 counts a
        // 3-byte char as 0.75 tokens, so Chinese-heavy transcripts trigger
        // compaction too late and slam into the provider context limit.
        let messages = vec![Message::text(Role::User, "修".repeat(1000))];
        let est = estimate_tokens(&messages);
        assert!(est >= 950, "CJK estimate too low ({est} for 1000 chars)");
    }

    #[test]
    fn ascii_text_stays_at_a_quarter_byte_per_token() {
        let messages = vec![Message::text(Role::User, "a".repeat(1000))];
        let est = estimate_tokens(&messages);
        assert!(
            (200..=300).contains(&est),
            "ASCII estimate drifted from ~len/4: {est}"
        );
    }
}

#[cfg(test)]
mod span_tests {
    use super::*;
    use leveler_model::Role;

    fn msg(role: Role, text: &str) -> Message {
        Message::text(role, text)
    }

    fn assistant_call(id: &str, name: &str) -> Message {
        Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: leveler_model::ToolCall {
                    id: leveler_core::ToolCallId::new(id),
                    name: name.into(),
                    arguments: Default::default(),
                },
            }],
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            origin: None,
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                result: leveler_model::ToolResultContent {
                    call_id: leveler_core::ToolCallId::new(id),
                    content: "ok".into(),
                    is_error: false,
                },
            }],
        }
    }

    // ── Round boundaries: a tool result never travels without its call ──────

    #[test]
    fn round_boundary_backs_up_to_the_owning_assistant_call() {
        let msgs = vec![
            msg(Role::User, "task"),
            assistant_call("c1", "read"),
            tool_result("c1"),
            tool_result("c1"),
        ];
        // Cutting onto the second result must include the assistant that owns it.
        let start = round_boundary(&msgs, 3);
        assert_eq!(start, 1);
        assert_eq!(msgs[start].role, Role::Assistant);
    }

    #[test]
    fn round_boundary_keeps_a_multi_call_exchange_whole() {
        let mut call = assistant_call("a", "read");
        call.content.extend(assistant_call("b", "read").content);
        let msgs = vec![
            msg(Role::User, "task"),
            call,
            tool_result("a"),
            tool_result("b"),
            msg(Role::Assistant, "done"),
        ];
        // A cut on either result moves back to the call that opened them.
        assert_eq!(round_boundary(&msgs, 2), 1);
        assert_eq!(round_boundary(&msgs, 3), 1);
        leveler_model::validate_tool_exchange(&msgs[round_boundary(&msgs, 3)..])
            .expect("the trimmed tail keeps the exchange whole");
        // A cut that is already on a round boundary stays put.
        assert_eq!(round_boundary(&msgs, 4), 4);
    }

    /// A result whose call is not in the input at all cannot be paired by
    /// moving the cut. The boundary does not skip it — skipping would drop
    /// history nobody can account for — so the sequence stays invalid and the
    /// provider boundary refuses it.
    #[test]
    fn round_boundary_never_skips_a_result_it_cannot_pair() {
        let msgs = vec![tool_result("c1"), msg(Role::User, "task")];
        assert_eq!(round_boundary(&msgs, 0), 0);
        assert_eq!(round_boundary(&msgs, 1), 1);
    }

    /// Every count window and token budget, over a transcript mixing
    /// single- and multi-call rounds with large results: the fold keeps each
    /// exchange whole, so its output always satisfies the provider-bound
    /// tool-exchange invariant.
    #[test]
    fn every_fold_of_a_tool_transcript_keeps_exchanges_whole() {
        let mut msgs = vec![msg(Role::System, "sys"), msg(Role::User, "task")];
        for round in 0..8 {
            let ids: Vec<String> = (0..=round % 3).map(|k| format!("r{round}_{k}")).collect();
            let mut call = assistant_call(&ids[0], "read");
            for id in &ids[1..] {
                call.content.extend(assistant_call(id, "read").content);
            }
            msgs.push(call);
            // Alternate one grouped result message with one message per result.
            if round % 2 == 0 {
                let mut grouped = tool_result(&ids[0]);
                for id in &ids[1..] {
                    grouped.content.extend(tool_result(id).content);
                }
                msgs.push(grouped);
            } else {
                msgs.extend(ids.iter().map(|id| tool_result(id)));
            }
            if round == 4 {
                msgs.push(msg(Role::Assistant, "midway"));
                msgs.push(msg(Role::User, "keep going"));
            }
        }
        msgs.push(msg(Role::Assistant, "done"));
        leveler_model::validate_tool_exchange(&msgs).expect("the input is valid");

        for keep in 1..=msgs.len() {
            for tokens in [0u64, 1, 5, 20, 80] {
                let folded = compact_messages(&msgs, keep, tokens, Some("brief"), Some("task"));
                if let Err(violation) = leveler_model::validate_tool_exchange(&folded) {
                    panic!("keep={keep} tokens={tokens}: {violation}");
                }
            }
        }
    }

    #[test]
    fn compaction_tail_never_begins_on_a_tool_result() {
        // A one-message tail would land exactly on the result; the span backs up.
        let msgs = vec![
            msg(Role::User, "task"),
            msg(Role::Assistant, "a"),
            msg(Role::User, "u"),
            assistant_call("c1", "read"),
            tool_result("c1"),
        ];
        let (_, tail_start) = compaction_span(&msgs, 1, 0).expect("a foldable middle");
        assert_ne!(msgs[tail_start].role, Role::Tool);
    }

    #[test]
    fn token_cap_disabled_keeps_last_n_by_count() {
        // keep_recent_tokens = 0 → pure message-count behavior (engine path).
        let mut msgs = vec![msg(Role::User, "task")];
        for i in 0..20 {
            msgs.push(msg(Role::Assistant, &format!("m{i}")));
        }
        let (head_end, tail_start) = compaction_span(&msgs, 12, 0).unwrap();
        assert_eq!(head_end, 1);
        assert_eq!(
            tail_start,
            msgs.len() - 12,
            "tail should be exactly last 12"
        );
    }

    #[test]
    fn oversized_recent_output_is_dropped_from_the_retained_tail() {
        // A single huge tool output inside the last `keep_recent` messages must
        // be folded into the summarized middle, not kept verbatim — otherwise the
        // fold stays over the window and compaction achieves nothing.
        const BUDGET: u64 = 8_000;
        let mut msgs = vec![msg(Role::User, "task")];
        for i in 0..20 {
            msgs.push(msg(Role::Assistant, &format!("small {i}")));
        }
        // ~ (BUDGET * 8) / 4 tokens ≫ BUDGET, sitting inside the last 12 messages.
        msgs.push(msg(Role::Assistant, &"x".repeat(BUDGET as usize * 8)));
        for i in 0..3 {
            msgs.push(msg(Role::Assistant, &format!("tail {i}")));
        }

        // Without the cap the last 12 include the giant and blow the budget…
        let (_, uncapped) = compaction_span(&msgs, 12, 0).unwrap();
        assert!(
            estimate_tokens(&msgs[uncapped..]) > BUDGET,
            "precondition: uncapped tail should exceed the budget"
        );

        // …with the cap the retained tail fits, and the giant sits before it.
        let (_, tail_start) = compaction_span(&msgs, 12, BUDGET).unwrap();
        assert!(
            estimate_tokens(&msgs[tail_start..]) <= BUDGET,
            "retained tail still exceeds budget: {}",
            estimate_tokens(&msgs[tail_start..])
        );
    }

    #[test]
    fn token_cap_always_keeps_the_newest_message() {
        // Even a single message larger than the budget must be retained — it is
        // usually what just overflowed and the model needs it.
        let msgs = vec![
            msg(Role::User, "task"),
            msg(Role::Assistant, "a"),
            msg(Role::Assistant, &"x".repeat(100_000)),
        ];
        let span = compaction_span(&msgs, 12, 1_000);
        // Nothing meaningful to fold (middle < 2) → None, and we never panic
        // trying to walk past the last message.
        assert!(span.is_none() || span.unwrap().1 == msgs.len() - 1);
    }

    #[test]
    fn update_prompt_selected_only_when_a_prior_briefing_is_present() {
        let fresh = vec![
            msg(Role::User, "task"),
            msg(Role::Assistant, "did some work"),
        ];
        assert_eq!(summary_prompt_for(&fresh), COMPACT_PROMPT);

        // A range that already carries a fold breadcrumb takes the UPDATE path.
        let with_prior = vec![
            msg(Role::User, "task"),
            msg(
                Role::User,
                &format!("{COMPACTION_BREADCRUMB_MARKER} to fit the window: 9 steps elided.]"),
            ),
            msg(Role::Assistant, "more work"),
        ];
        assert_eq!(summary_prompt_for(&with_prior), COMPACT_UPDATE_PROMPT);
    }

    #[test]
    fn fold_breadcrumb_carries_the_detection_marker() {
        // The breadcrumb compact_messages writes must contain the exact marker
        // summary_prompt_for keys off, or repeated folds silently lose the
        // incremental-update path.
        let mut msgs = vec![msg(Role::System, "sys"), msg(Role::User, "task")];
        for i in 0..10 {
            msgs.push(msg(Role::Assistant, &format!("step {i}")));
        }
        let out = compact_messages(&msgs, 4, 0, Some("a briefing"), None);
        assert!(
            out.iter()
                .any(|m| m.text_content().contains(COMPACTION_BREADCRUMB_MARKER)),
            "fold breadcrumb no longer contains the detection marker"
        );
    }
}

#[cfg(test)]
mod fold_requirement_tests {
    use super::FoldRequirement;

    /// The boundary between soft and hard is the CAPACITY, and it is inclusive:
    /// a request that exactly fits may still be sent unfolded.
    #[test]
    fn the_capacity_boundary_is_inclusive() {
        assert_eq!(
            FoldRequirement::classify(1_000, 500, Some(1_000)),
            FoldRequirement::Soft
        );
        assert_eq!(
            FoldRequirement::classify(1_001, 500, Some(1_000)),
            FoldRequirement::HardRequired
        );
    }

    /// Under the quality boundary nothing happens; over it the fold is a
    /// quality choice.
    #[test]
    fn the_quality_boundary_separates_none_from_soft() {
        assert_eq!(
            FoldRequirement::classify(500, 500, Some(10_000)),
            FoldRequirement::None
        );
        assert_eq!(
            FoldRequirement::classify(501, 500, Some(10_000)),
            FoldRequirement::Soft
        );
    }

    /// With no declared window there is no capacity to exceed: every fold is a
    /// quality choice and none is mandatory.
    #[test]
    fn an_unknown_capacity_never_requires_a_fold() {
        assert_eq!(
            FoldRequirement::classify(u64::MAX - 1, 500, None),
            FoldRequirement::Soft
        );
    }
}
