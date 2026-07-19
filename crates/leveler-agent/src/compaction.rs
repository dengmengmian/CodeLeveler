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

/// The head/middle/tail split for compaction: `(head_end, tail_start)`, or None
/// when there is nothing worth folding. Cuts only at round boundaries so a
/// tool-call is never separated from its tool-result (the provider rejects
/// orphaned tool calls).
pub(crate) fn compaction_span(messages: &[Message], keep_recent: usize) -> Option<(usize, usize)> {
    // Head: the system prompt(s) plus the first user message (the task anchor).
    let head_end = messages
        .iter()
        .position(|m| m.role == Role::User)
        .map(|i| i + 1)
        .unwrap_or(0);

    // Tail start: keep the last `keep_recent` messages, but never begin the tail
    // on a Tool result — back up to its owning assistant so the pair stays whole.
    let mut tail_start = messages.len().saturating_sub(keep_recent).max(head_end);
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
    summary: Option<&str>,
    active_objective: Option<&str>,
) -> Vec<Message> {
    let pin = active_objective
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(objective_pin_message);

    let Some((head_end, tail_start)) = compaction_span(messages, keep_recent) else {
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
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<String> {
    // Advisory call: never let a slow summarizer stall the main loop.
    const SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    let (_, tail_start) = compaction_span(messages, keep_recent)?;
    let mut summary_messages = messages[..tail_start].to_vec();
    summary_messages.push(Message::text(Role::User, COMPACT_PROMPT));

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
