//! Offline context replay (PR5 / PR5c measurement).
//!
//! Replays a persisted transcript (`session_messages.payload` rows, exported
//! as one JSON array) round by round through the REAL owners — the request
//! projection, the context accountant and the compaction fold — and prints the
//! per-round breakdown plus summary statistics. No provider is contacted, so
//! two policies can be compared on the same transcript without model variance.
//!
//! Export a transcript with:
//!
//! ```text
//! sqlite3 sessions.db "select json_group_array(json(payload)) from \
//!   (select payload from session_messages where session_id='<id>' order by ordinal);" \
//!   > transcript.json
//! ```
//!
//! Usage:
//!
//! ```text
//! context_replay <transcript.json> [flags]
//!   --replay never|when_tools|always   route reasoning-replay scope (default never)
//!   --signed-blocks                    route carries authenticated reasoning blocks
//!   --empty-key                        a replayed turn with no reasoning sends ""
//!   --retention all|none|last-N        requested reasoning retention (default all)
//!   --window N                         model context window (0 = unknown)
//!   --reliable N                       model quality boundary (0 = undeclared)
//!   --reservation N                    output reservation per request
//!   --headroom N                       extra pressure headroom
//!   --keep-messages N                  fold tail bound in messages
//!   --keep-tokens N                    fold tail bound in tokens (absolute)
//!   --keep-token-divisor N             fold tail token budget = threshold / N
//!   --fold                             apply the auto-compaction fold
//!   --fold-budget N                    fold when the projected request exceeds N
//!                                      (0 = the resolved pressure threshold)
//!   --base-fixed N                     control context + tool definitions cost
//! ```
//!
//! `--base-fixed` is the part of a real request that is not in the transcript:
//! the control context and the tool definitions. It is added to every round's
//! projected input so the numbers are comparable with provider-reported input
//! tokens.
//!
//! # Compaction cost
//!
//! A fold is itself a model call: the pre-fold context is sent as the
//! summarizer input, and the briefing is the output. This harness measures the
//! input exactly (the projection of the messages the summarizer is handed) and
//! models the output with a documented size model, because no provider is
//! contacted offline. The model is stated in [`summary_output_tokens`]; the
//! real figure must come from a real compaction call.

use leveler_context::compact_messages;
use leveler_model::{
    ContentPart, ContextAccounting, Message, ReasoningReplayContract, ReasoningReplayScope,
    ReasoningRetention, RequestProjection, ToolDefinition,
};

/// A stand-in for the summarizer instruction's token cost offline. The real
/// prompt lives in `leveler_context` and is added to the summarizer request in
/// production; this harness adds a comparable fixed cost so the compaction
/// input is not understated.
const SUMMARY_PROMPT_TOKENS: u64 = 400;

struct Config {
    transcript: String,
    contract: ReasoningReplayContract,
    retention: ReasoningRetention,
    window: u32,
    reliable: u32,
    reservation: u32,
    headroom: u32,
    keep_messages: usize,
    keep_tokens: u64,
    keep_token_divisor: u64,
    fold: bool,
    fold_budget: u64,
    base_fixed: u64,
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse(args: &[String]) -> Config {
    let transcript = args
        .get(1)
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .expect("usage: context_replay <transcript.json> [flags]");
    let scope = match arg_value(args, "--replay").as_deref() {
        None | Some("never") => ReasoningReplayScope::Never,
        Some("when_tools") => ReasoningReplayScope::WhenToolsPresent,
        Some("always") => ReasoningReplayScope::Always,
        Some(other) => panic!("unknown --replay {other}"),
    };
    let retention = match arg_value(args, "--retention").as_deref() {
        None | Some("all") => ReasoningRetention::All,
        Some("none") => ReasoningRetention::None,
        Some(other) => {
            let n = other
                .strip_prefix("last-")
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("unknown --retention {other}"));
            ReasoningRetention::LastTurns(n)
        }
    };
    let number = |name: &str| -> u32 {
        arg_value(args, name)
            .map(|v| v.parse().expect("numeric flag"))
            .unwrap_or(0)
    };
    Config {
        transcript,
        contract: if args.iter().any(|a| a == "--signed-blocks") {
            ReasoningReplayContract::signed_block()
        } else if scope == ReasoningReplayScope::Never {
            ReasoningReplayContract::NONE
        } else {
            ReasoningReplayContract::raw_field(
                scope,
                if args.iter().any(|a| a == "--empty-key") {
                    leveler_model::MissingReasoningReplay::EmptyString
                } else {
                    leveler_model::MissingReasoningReplay::Omit
                },
            )
        },
        retention,
        window: number("--window"),
        reliable: number("--reliable"),
        reservation: number("--reservation"),
        headroom: number("--headroom"),
        keep_messages: arg_value(args, "--keep-messages")
            .map(|v| v.parse().unwrap())
            .unwrap_or(leveler_context::COMPACT_KEEP_RECENT),
        keep_tokens: arg_value(args, "--keep-tokens")
            .map(|v| v.parse().unwrap())
            .unwrap_or(0),
        keep_token_divisor: arg_value(args, "--keep-token-divisor")
            .map(|v| v.parse().unwrap())
            .unwrap_or(2),
        fold: args.iter().any(|a| a == "--fold"),
        fold_budget: arg_value(args, "--fold-budget")
            .map(|v| v.parse().unwrap())
            .unwrap_or(0),
        base_fixed: arg_value(args, "--base-fixed")
            .map(|v| v.parse().unwrap())
            .unwrap_or(0),
    }
}

/// The threshold the harness resolves: `min(quality_boundary, capacity)`, where
/// `capacity = window - reservation - headroom`. `0` = folding disabled.
fn pressure_threshold(config: &Config) -> u32 {
    if config.window == 0 {
        return 0;
    }
    let reservation = config.reservation;
    let capacity = config
        .window
        .saturating_sub(reservation)
        .saturating_sub(config.headroom);
    let quality = if config.reliable == 0 {
        capacity
    } else {
        config.reliable
    };
    quality.min(capacity).max(1)
}

const MUTATING: &[&str] = &[
    "apply_patch",
    "write_file",
    "edit_file",
    "run_command",
    "shell_command",
];

/// One slice of the accounting's breakdown, by stable name. Top-level and
/// message children share names, so both are searched.
fn category(accounting: &ContextAccounting, name: &str) -> u64 {
    fn find(categories: &[leveler_model::ContextCategory], name: &str) -> Option<u64> {
        for category in categories {
            if category.name == name {
                return Some(category.tokens);
            }
            if let Some(found) = find(&category.children, name) {
                return Some(found);
            }
        }
        None
    }
    find(&accounting.categories, name).unwrap_or(0)
}

/// One placeholder tool, so `ReasoningReplayScope::WhenToolsPresent` resolves
/// exactly as it does in a real coding request. Its token cost is subtracted
/// again and belongs to `--base-fixed`.
fn placeholder_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "read_file".to_string(),
        description: String::new(),
        input_schema: serde_json::json!({}),
    }]
}

fn carries_mutation(message: &Message) -> bool {
    message.content.iter().any(|part| match part {
        ContentPart::ToolCall { call } => MUTATING.contains(&call.name.as_str()),
        _ => false,
    })
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

/// The OFFLINE size model for a compaction briefing, in estimated tokens.
///
/// No provider is contacted here, so the summary's real length is unknowable.
/// The model is a documented stand-in, not a measurement: a briefing that keeps
/// the load-bearing facts of the elided rounds tends to land near an eighth of
/// what it replaces, bounded so a huge fold cannot claim a huge briefing. It is
/// applied to BOTH the one-time output cost and the summary row retained in the
/// after-state, so the two are consistent. Replace it with a measured figure
/// from a real compaction call before trusting small differences between arms.
fn summary_output_tokens(elided_tokens: u64) -> u64 {
    (elided_tokens / 8).clamp(64, 2_048)
}

/// A placeholder briefing of approximately `tokens` estimated tokens. Offline
/// only: the content is never shown to a model, so only its size is modeled.
fn placeholder_summary(tokens: u64) -> String {
    "briefing ".repeat((tokens.max(1) as usize) / 2 + 1)
}

/// The shape of the active surface: what a fold actually released.
#[derive(Default, Clone, Copy)]
struct Shape {
    assistant_turns: usize,
    reasoning_turns: usize,
    tool_exchanges: usize,
    tool_result_tokens: u64,
    summary_tokens: u64,
}

fn shape(
    messages: &[Message],
    contract: ReasoningReplayContract,
    retention: ReasoningRetention,
    tools: &[ToolDefinition],
) -> Shape {
    let projection = RequestProjection::project(messages, tools, contract, retention);
    let mut shape = Shape::default();
    let mut open_calls: Vec<String> = Vec::new();
    for message in projection.messages() {
        if message.role == leveler_model::Role::Assistant {
            shape.assistant_turns += 1;
            if message.reasoning.is_present() {
                shape.reasoning_turns += 1;
            }
        }
        for part in &message.content {
            match part {
                ContentPart::ToolCall { call } => open_calls.push(call.id.to_string()),
                ContentPart::ToolResult { result } => {
                    let id = result.call_id.to_string();
                    if open_calls.contains(&id) {
                        shape.tool_exchanges += 1;
                        shape.tool_result_tokens += leveler_model::estimate_text(&result.content);
                    }
                }
                ContentPart::Text { text }
                    if message.role == leveler_model::Role::User
                        && text.contains(leveler_model::COMPACTION_BREADCRUMB_MARKER) =>
                {
                    shape.summary_tokens += leveler_model::estimate_text(text);
                }
                _ => {}
            }
        }
    }
    shape
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let config = parse(&args);
    let raw = std::fs::read_to_string(&config.transcript).expect("read transcript");
    let transcript: Vec<Message> =
        serde_json::from_str(&raw).expect("transcript is a Message array");

    let threshold = if config.fold_budget > 0 {
        config.fold_budget
    } else {
        u64::from(pressure_threshold(&config))
    };
    let keep_recent_tokens = if config.keep_tokens > 0 {
        config.keep_tokens
    } else if config.keep_token_divisor > 0 {
        threshold / config.keep_token_divisor
    } else {
        0
    };
    let tools = placeholder_tools();
    let placeholder_cost = leveler_model::estimate_tool_definitions(&tools);

    // Round i is the request that produced the i-th assistant message:
    // everything before it. Folding mirrors the executor: when the projected
    // request crosses the budget, fold the in-memory transcript before the next
    // request.
    let mut in_memory = Vec::<Message>::new();
    let mut compactions = 0usize;
    let mut compaction_input_total = 0u64;
    let mut compaction_output_total = 0u64;
    let mut rows: Vec<(usize, u64, u64, u64, u64, u64, u64, u64)> = Vec::new();
    let mut breakdowns: Vec<[u64; 6]> = Vec::new();
    let mut first_edit: Option<usize> = None;
    let mut saw_edit = false;
    let mut out = String::new();

    for (index, message) in transcript.iter().enumerate() {
        let is_assistant = message.role == leveler_model::Role::Assistant;
        if !is_assistant {
            in_memory.push(message.clone());
            if carries_mutation(message) {
                saw_edit = true;
            }
            continue;
        }
        // The request that produced this assistant turn.
        let projection =
            RequestProjection::project(&in_memory, &tools, config.contract, config.retention);
        let accounting = ContextAccounting::compute(
            leveler_model::ModelRef::new("replay", "offline"),
            &projection,
            Some(config.window),
            (threshold > 0).then_some(threshold as u32),
            None,
        );
        let reasoning = accounting.projected_reasoning_tokens();
        let tool_results = category(&accounting, "tool_results");
        let visible = category(&accounting, "user") + category(&accounting, "assistant");
        let tool_calls = category(&accounting, "tool_calls");
        let control = category(&accounting, "system");
        let tool_schemas = category(&accounting, "tool_definitions");
        let total = accounting
            .used_tokens
            .saturating_sub(placeholder_cost)
            .saturating_add(config.base_fixed);
        rows.push((
            index,
            total,
            reasoning,
            tool_results,
            compactions as u64,
            projection.summary().carried_turns as u64,
            projection.summary().protocol_protected_turns as u64,
            compaction_input_total,
        ));
        breakdowns.push([
            control,
            tool_schemas,
            visible,
            reasoning,
            tool_calls,
            tool_results,
        ]);
        if saw_edit && first_edit.is_none() {
            first_edit = Some(rows.len() - 1);
        }

        // A mutating tool call is an assistant turn, so it is recorded here
        // (not in the non-assistant branch above, which cannot see one).
        if carries_mutation(message) {
            saw_edit = true;
        }
        in_memory.push(message.clone());

        if config.fold && threshold > 0 && total > threshold {
            // The summarizer request replays no reasoning (it exposes no
            // tools), so its input is the projection of the range with an
            // empty tool surface plus the briefing prompt. The span owner
            // decides the range, exactly as production does.
            let mut summary_input = in_memory.clone();
            summary_input.push(Message::text(
                leveler_model::Role::User,
                "context checkpoint compaction briefing instruction",
            ));
            let input_tokens =
                RequestProjection::project(&summary_input, &[], config.contract, config.retention)
                    .estimated_tokens()
                    .saturating_add(SUMMARY_PROMPT_TOKENS);
            let elided = leveler_model::estimate_tokens(&in_memory);
            let output_tokens = summary_output_tokens(elided);
            let summary = placeholder_summary(output_tokens);
            let folded = compact_messages(
                &in_memory,
                config.keep_messages,
                keep_recent_tokens,
                Some(&summary),
                None,
            );
            if folded.len() < in_memory.len() {
                compactions += 1;
                compaction_input_total = compaction_input_total.saturating_add(input_tokens);
                compaction_output_total = compaction_output_total.saturating_add(output_tokens);
                in_memory = folded;
            }
        }
    }

    let sampled: Vec<usize> = {
        let mut picks = vec![0usize, 4, 9];
        if let Some(edit) = first_edit {
            picks.push(edit);
            picks.push(edit + 1);
            picks.push(rows.len().saturating_sub(1));
        } else {
            picks.push(rows.len().saturating_sub(1));
        }
        picks.retain(|p| *p < rows.len());
        picks.sort_unstable();
        picks.dedup();
        picks
    };
    let final_shape = shape(&in_memory, config.contract, config.retention, &tools);

    let keep_label = if keep_recent_tokens > 0 {
        keep_recent_tokens.to_string()
    } else {
        "0".to_string()
    };
    out.push_str(&format!(
        "CONTEXT_REPLAY {}  retention={} contract={} fold={} budget={threshold} \
         keep_messages={} keep_tokens={keep_label} base_fixed={}\n\
         round total control tools visible reasoning tool_calls tool_results \
         compactions carried_turns protected_turns\n",
        config.transcript,
        config.retention.arm_name(),
        config.contract.arm_name(),
        config.fold,
        config.keep_messages,
        config.base_fixed,
    ));
    for pick in &sampled {
        let (index, total, _reasoning, _tool, folds, carried, protected, _cinput) = rows[*pick];
        let [control, schemas, visible, reasoning_break, calls, results] = breakdowns[*pick];
        out.push_str(&format!(
            "SAMPLE round={} msg_index={index} total={total} control={control} tools={schemas} \
             visible={visible} reasoning={reasoning_break} tool_calls={calls} \
             tool_results={results} compaction=0 compactions={folds} \
             carried_turns={carried} protected_turns={protected}\n",
            pick + 1
        ));
    }

    let totals: Vec<u64> = rows.iter().map(|r| r.1).collect();
    let mut sorted = totals.clone();
    sorted.sort_unstable();
    let reasoning_sum: u64 = rows.iter().map(|r| r.2).sum();
    let tool_sum: u64 = rows.iter().map(|r| r.3).sum();
    let grand_total: u64 = totals.iter().sum();
    let combined = grand_total
        .saturating_add(compaction_input_total)
        .saturating_add(compaction_output_total);
    let mean = if totals.is_empty() {
        0
    } else {
        grand_total / totals.len() as u64
    };
    out.push_str(&format!(
        "SUMMARY rounds={} mean={mean} median={} p90={} max={} total={grand_total} \
         reasoning_replay_total={reasoning_sum} reasoning_share={:.3} tool_result_total={tool_sum} \
         compactions={compactions} compaction_input_total={compaction_input_total} \
         compaction_output_total={compaction_output_total} combined_total={combined}\n",
        totals.len(),
        percentile(&sorted, 0.5),
        percentile(&sorted, 0.9),
        sorted.last().copied().unwrap_or(0),
        if grand_total == 0 {
            0.0
        } else {
            reasoning_sum as f64 / grand_total as f64
        },
    ));
    out.push_str(&format!(
        "SHAPE assistant_turns={} reasoning_turns={} tool_exchanges={} tool_result_tokens={} \
         summary_tokens={}\n",
        final_shape.assistant_turns,
        final_shape.reasoning_turns,
        final_shape.tool_exchanges,
        final_shape.tool_result_tokens,
        final_shape.summary_tokens,
    ));
    print!("{out}");
    let report = std::path::Path::new("/tmp/cl-context-audit/replay-report.txt");
    let _ = std::fs::create_dir_all(report.parent().unwrap());
    let _ = std::fs::write(report, &out);
}
