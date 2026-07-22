//! Anchored context compaction: span selection, folding, token estimate.

use leveler_model::{ContentPart, Message, Role};

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
pub(crate) const COMPACTION_BREADCRUMB_MARKER: &str = "[Earlier context was compacted";

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
    while tail_start > head_end && messages[tail_start].role == Role::Tool {
        tail_start -= 1;
    }

    // Nothing meaningful in the middle → leave it alone.
    if tail_start <= head_end || tail_start - head_end < 2 {
        return None;
    }
    Some((head_end, tail_start))
}

/// Marker for the host-pinned active objective re-injected after compaction.
pub(crate) const ACTIVE_OBJECTIVE_MARKER: &str = "[Active objective — host-pinned]";

/// Build the user message that re-pins the host objective after a fold.
pub(crate) fn objective_pin_message(objective: &str) -> Message {
    let obj = objective.trim();
    Message {
        role: Role::User,
        content: vec![ContentPart::Text {
            text: format!(
                "{ACTIVE_OBJECTIVE_MARKER}\n\
                 <objective>\n{obj}\n</objective>\n\
                 This is the only active request for this turn. Do not resurrect \
                 earlier questions that were already answered."
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
             Summary of the elided work — build on it, do not redo it:\n{summary}\n\n\
             [Continue from the messages below.]"
        ),
        None => format!(
            "[Earlier context was compacted to fit the window: {elided} steps elided.{files_note} \
             Summarization was unavailable, so the details of those steps are LOST — \
             re-establish any fact you need with tools instead of assuming it. \
             Continue from the messages below.]"
        ),
    };
    let breadcrumb = Message {
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

/// Ask `runtime` for a compaction handoff briefing over the middle the fold
/// is about to elide. Returns `None` when there is nothing to fold or the
/// call fails/times out — callers then fold with a bare breadcrumb, which
/// still beats overflowing the window (and the loss stays explicit).
pub async fn summarize_with_model(
    runtime: &dyn leveler_model::ModelRuntime,
    model: &leveler_model::ModelRef,
    reasoning_effort: Option<leveler_model::ReasoningEffort>,
    messages: &[Message],
    keep_recent: usize,
    keep_recent_tokens: u64,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<String> {
    // Advisory call: never let a slow summarizer stall the main loop.
    const SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    let (_, tail_start) = compaction_span(messages, keep_recent, keep_recent_tokens)?;
    let to_summarize = &messages[..tail_start];
    let mut summary_messages = to_summarize.to_vec();
    summary_messages.push(Message::text(Role::User, summary_prompt_for(to_summarize)));

    let mut request = leveler_model::ModelRequest::new(model.clone(), summary_messages);
    request.tool_choice = leveler_model::ToolChoice::None;
    request.max_output_tokens = Some(1024);
    request.reasoning_effort = reasoning_effort;

    let response = tokio::time::timeout(
        SUMMARY_TIMEOUT,
        runtime.generate(request, cancellation.child_token()),
    )
    .await
    .ok()?
    .ok()?;
    let text = response.message.text_content().trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Coarse token estimate over a transcript's textual content. A fallback for
/// providers/gateways that don't report streaming usage, so auto-compaction
/// still triggers on a growing conversation.
///
/// ASCII averages ~4 bytes/token; CJK and other non-ASCII text spends ~1 token
/// per character (~3 UTF-8 bytes), so those bytes are weighted at 3 bytes/token
/// — a flat ÷4 under-counts Chinese-heavy transcripts by ~25% and fires
/// compaction only after the request already exceeds the provider window.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    // A conservative flat cost (in ASCII byte-equivalents, ÷4 below) for one
    // image, so a vision turn isn't counted as ~free. Real vision billing is
    // tile-based and model-specific; ~1000 tokens/image is a safe floor that
    // keeps compaction firing on image-heavy conversations when the gateway
    // reports no usage.
    const IMAGE_BYTE_EQUIV: u64 = 4096;
    let mut ascii_bytes: u64 = 0;
    let mut wide_bytes: u64 = 0;
    let mut flat: u64 = 0;
    let mut count = |s: &str| {
        let ascii = s.bytes().filter(u8::is_ascii).count() as u64;
        ascii_bytes += ascii;
        wide_bytes += s.len() as u64 - ascii;
    };
    for part in messages.iter().flat_map(|m| &m.content) {
        match part {
            ContentPart::Text { text } => count(text),
            ContentPart::ToolCall { call } => {
                count(&call.name);
                count(&call.arguments.to_string());
            }
            ContentPart::ToolResult { result } => count(&result.content),
            ContentPart::Image { .. } => flat += IMAGE_BYTE_EQUIV / 4,
            _ => {}
        }
    }
    ascii_bytes / 4 + wide_bytes / 3 + flat
}

#[cfg(test)]
mod estimate_tests {
    use super::*;
    use leveler_model::Role;

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

    #[test]
    fn token_cap_disabled_keeps_last_n_by_count() {
        // keep_recent_tokens = 0 → pure message-count behavior (engine path).
        let mut msgs = vec![msg(Role::User, "task")];
        for i in 0..20 {
            msgs.push(msg(Role::Assistant, &format!("m{i}")));
        }
        let (head_end, tail_start) = compaction_span(&msgs, 12, 0).unwrap();
        assert_eq!(head_end, 1);
        assert_eq!(tail_start, msgs.len() - 12, "tail should be exactly last 12");
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
