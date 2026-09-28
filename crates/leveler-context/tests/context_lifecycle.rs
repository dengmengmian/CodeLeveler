//! The context lifecycle at the transcript level: which context survives, in
//! what form, as a session grows.
//!
//! These tests drive the REAL owners — `leveler_model::RequestProjection` for
//! "what the provider sees" and `leveler_context::compact_messages` for the
//! fold — over synthetic transcripts. No provider is contacted and no model
//! judgement is involved, so every figure here is the production estimator's.
//!
//! The policy under test, stated once:
//!
//! * what a request carries is decided by the projection
//!   ([`leveler_model::RequestProjection`]) from facts — the route's resolved
//!   replay contract and the requested retention arm;
//! * a route that replays captured reasoning replays the full reasoning of
//!   every retained assistant turn, and the requested arm cannot drop one while
//!   its turn stays in the active surface;
//! * the context lifecycle bounds the surface: a fold releases the rounds it
//!   elides, including their reasoning, and keeps the working set verbatim;
//! * the fold never touches the raw transcript — the durable rows are the ones
//!   the runtime loaded, and only the active surface shrinks.

use leveler_context::{compact_messages, estimate_tokens};
use leveler_core::ToolCallId;
use leveler_model::{
    ContentPart, Message, MissingReasoningReplay, PromptAuthority, PromptSource,
    ReasoningReplayContract, ReasoningReplayScope, ReasoningRetention, RequestProjection, Role,
    ToolCall, ToolDefinition, ToolResultContent,
};

fn route() -> ReasoningReplayContract {
    ReasoningReplayContract::raw_field(
        ReasoningReplayScope::WhenToolsPresent,
        MissingReasoningReplay::EmptyString,
    )
}

fn tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "read_file".to_string(),
        description: "read a file".to_string(),
        // The schema's own size is priced by `prompt_surface_budget`; this
        // fixture only needs a tool to exist for the route to be tool-bearing.
        input_schema: Default::default(),
    }]
}

/// One round: an assistant turn that thought and called a tool, plus its result.
fn thought_round(index: usize, thinking: &str, result: &str) -> Vec<Message> {
    let call_id = ToolCallId::new(format!("call-{index}"));
    vec![
        Message::from_parts(
            Role::Assistant,
            vec![
                ContentPart::Reasoning {
                    text: thinking.to_string(),
                },
                ContentPart::ToolCall {
                    call: ToolCall {
                        id: call_id.clone(),
                        name: "read_file".into(),
                        arguments: Default::default(),
                    },
                },
            ],
            None,
        ),
        Message::from_parts(
            Role::Tool,
            vec![ContentPart::ToolResult {
                result: ToolResultContent {
                    call_id,
                    content: result.to_string(),
                    is_error: false,
                },
            }],
            None,
        ),
    ]
}

fn transcript(rounds: usize, thinking_len: usize, result_len: usize) -> Vec<Message> {
    let mut messages = vec![
        Message::text(Role::System, "you are a coding agent"),
        Message::user_input("fix the bug"),
    ];
    for index in 0..rounds {
        let thinking = format!("thinking-{index} {}", "t".repeat(thinking_len));
        let result = format!("result-{index} {}", "r".repeat(result_len));
        messages.extend(thought_round(index, &thinking, &result));
    }
    messages
}

/// Everything the provider would receive as reasoning on this request, at the
/// retention and contract under test.
fn replayed_reasoning(
    messages: &[Message],
    tools: &[ToolDefinition],
    contract: ReasoningReplayContract,
    retention: ReasoningRetention,
) -> Vec<String> {
    RequestProjection::project(messages, tools, contract, retention)
        .messages()
        .iter()
        .filter_map(|message| match &message.reasoning {
            leveler_model::ProjectedReasoning::Captured(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn replayed_reasoning_tokens(
    messages: &[Message],
    tools: &[ToolDefinition],
    retention: ReasoningRetention,
) -> u64 {
    RequestProjection::project(messages, tools, route(), retention).estimated_tokens()
}

/// R1: historical reasoning does not grow without bound across a session. It is
/// replayed while its rounds are in the active surface, and the fold releases
/// it together with the rounds it elides.
#[test]
fn historical_reasoning_is_released_by_the_fold_not_replayed_forever() {
    let tools = tools();
    let messages = transcript(10, 400, 100);

    let unfolded = RequestProjection::project(&messages, &tools, route(), ReasoningRetention::All);
    let unfolded_reasoning = unfolded
        .messages()
        .iter()
        .filter(|message| message.reasoning.is_present())
        .count();
    assert_eq!(
        unfolded_reasoning, 10,
        "every reasoning-bearing round in the active surface is replayed"
    );

    let folded = compact_messages(&messages, 4, 0, Some("briefing"), Some("fix the bug"));
    assert!(
        folded.len() < messages.len(),
        "the fold released something: {} -> {}",
        messages.len(),
        folded.len()
    );
    let projectable: Vec<Message> = folded.clone();
    let folded_reasoning =
        RequestProjection::project(&projectable, &tools, route(), ReasoningRetention::All);
    let carried = folded_reasoning
        .messages()
        .iter()
        .filter(|message| message.reasoning.is_present())
        .count();
    assert!(
        carried < unfolded_reasoning,
        "the elided rounds took their reasoning with them: {carried} carried of {unfolded_reasoning}"
    );
    let released: Vec<String> =
        replayed_reasoning(&messages, &tools, route(), ReasoningRetention::All);
    let kept: Vec<String> =
        replayed_reasoning(&projectable, &tools, route(), ReasoningRetention::All);
    for thinking in &kept {
        assert!(
            released.contains(thinking),
            "a retained round's reasoning is the same text it always was"
        );
    }
    assert!(
        !kept.iter().any(|text| text.starts_with("thinking-0 ")),
        "the oldest round's reasoning is no longer replayed: {kept:?}"
    );

    // A second fold of the already-folded surface bounds it again rather than
    // letting the earlier briefing turn back into replayed reasoning.
    let twice = compact_messages(
        &projectable,
        3,
        0,
        Some("updated briefing"),
        Some("fix the bug"),
    );
    let twice_carried =
        RequestProjection::project(&twice, &tools, route(), ReasoningRetention::All)
            .messages()
            .iter()
            .filter(|message| message.reasoning.is_present())
            .count();
    assert!(
        twice_carried <= carried,
        "repeated folding never regrows history"
    );
}

/// R2: the reasoning a replaying provider requires is still replayed. Dropping
/// it is not a saving the requested arm gets to take while the turn is retained.
#[test]
fn a_tool_exchange_keeps_the_reasoning_the_contract_requires() {
    let tools = tools();
    let messages = transcript(6, 300, 100);

    for retention in [
        ReasoningRetention::All,
        ReasoningRetention::LastTurns(1),
        ReasoningRetention::None,
    ] {
        let projection = RequestProjection::project(&messages, &tools, route(), retention);
        let summary = projection.summary();
        assert_eq!(
            summary.captured_turns, 6,
            "the transcript captures six reasoning turns"
        );
        assert_eq!(
            summary.carried_turns, 6,
            "every retained turn's reasoning is replayed ({retention:?})"
        );
        assert_eq!(
            summary.protocol_protected_turns,
            6 - retention.carried_positions(&messages).len(),
            "the arm's difference is reported, not absorbed ({retention:?})"
        );
        assert_eq!(
            replayed_reasoning(&messages, &tools, route(), retention).len(),
            6,
            "the retained reasoning is never stripped by the arm ({retention:?})"
        );
    }
}

/// R4: the arm cannot make a retained tool exchange provider-invalid. Under
/// every arm the projected sequence still answers every call exactly once, and
/// every retained assistant turn keeps the reasoning the route requires.
#[test]
fn the_arm_never_breaks_a_retained_tool_exchange() {
    use leveler_model::validate_tool_exchange;

    let tools = tools();
    let messages = transcript(4, 200, 80);
    for retention in [
        ReasoningRetention::All,
        ReasoningRetention::LastTurns(1),
        ReasoningRetention::None,
    ] {
        let projection = RequestProjection::project(&messages, &tools, route(), retention);
        let projected: Vec<Message> = projection
            .messages()
            .iter()
            .map(|message| {
                Message::from_parts(
                    message.role,
                    message.content.clone(),
                    message.origin.clone(),
                )
            })
            .collect();
        validate_tool_exchange(&projected)
            .unwrap_or_else(|violation| panic!("{retention:?}: {violation}"));
        let carried = projection
            .messages()
            .iter()
            .filter(|message| message.reasoning.is_present())
            .count();
        assert_eq!(
            carried, 4,
            "every retained turn's reasoning survives the arm ({retention:?})"
        );
    }
}

/// R3: a route with no replay channel sends none of it, and says so. Nothing
/// silently changes the meaning of the history for such a route.
#[test]
fn a_route_without_a_reasoning_channel_carries_none_and_reports_it() {
    let tools = tools();
    let messages = transcript(4, 300, 100);
    let projection = RequestProjection::project(
        &messages,
        &tools,
        ReasoningReplayContract::NONE,
        ReasoningRetention::All,
    );
    assert_eq!(projection.summary().captured_turns, 4);
    assert_eq!(projection.summary().carried_turns, 0);
    assert_eq!(
        projection.summary().protocol_protected_turns,
        0,
        "nothing is protected when there is no channel"
    );
    assert!(
        replayed_reasoning(
            &messages,
            &tools,
            ReasoningReplayContract::NONE,
            ReasoningRetention::All,
        )
        .is_empty()
    );
    // The transcript is unchanged for the next route that does have a channel.
    assert_eq!(
        messages
            .iter()
            .filter(|message| leveler_model::retention::carries_reasoning(message))
            .count(),
        4
    );
}

/// R4/R5: the fold's result is what a resumed run reloads. Elided reasoning is
/// gone from the active surface, retained reasoning is still replayed, and the
/// raw transcript rows are not what the fold rewrites.
#[test]
fn the_folded_surface_is_what_resume_reloads() {
    let tools = tools();
    let messages = transcript(8, 400, 100);
    let folded = compact_messages(&messages, 4, 0, Some("briefing"), Some("fix the bug"));

    let reloaded = RequestProjection::project(&folded, &tools, route(), ReasoningRetention::All);
    let reloaded_thinking: Vec<String> = reloaded
        .messages()
        .iter()
        .filter_map(|message| match &message.reasoning {
            leveler_model::ProjectedReasoning::Captured(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !reloaded_thinking
            .iter()
            .any(|text| text.starts_with("thinking-0 ")),
        "a resumed run cannot revive elided reasoning"
    );
    assert_eq!(
        reloaded_thinking.len(),
        reloaded
            .messages()
            .iter()
            .filter(|message| message.reasoning.is_present())
            .count()
    );

    // The raw rows the runtime loaded are exactly the ones it persisted: the
    // fold produced a new vector and touched nothing in place.
    assert_eq!(messages.len(), 2 + 8 * 2);
    assert!(
        messages
            .iter()
            .filter(|message| leveler_model::retention::carries_reasoning(message))
            .count()
            == 8,
        "the durable transcript still carries every captured reasoning block"
    );
}

/// A mid-transcript project-rule row is a standing constraint, not elidable
/// history: the fold carries it across the cut instead of summarizing it away.
#[test]
fn a_scoped_rule_row_survives_the_fold_that_elides_its_surroundings() {
    let mut messages = transcript(6, 200, 60);
    let rule = Message::text(Role::System, "scoped rule: never touch generated files");
    messages.insert(6, rule);
    let folded = compact_messages(&messages, 3, 0, Some("briefing"), Some("fix the bug"));
    assert!(
        folded.iter().any(|message| message.role == Role::System
            && message.text_content().contains("scoped rule")),
        "the scoped rule is carried: {:?}",
        folded
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>()
    );
    // It is carried as a standing row, not paraphrased into the briefing.
    let briefing = folded
        .iter()
        .find(|message| message.text_content().contains("compacted"))
        .expect("a briefing row");
    assert!(!briefing.text_content().contains("scoped rule"));
}

/// Tool evidence keeps its class across a fold. The newest observation stays
/// verbatim (it is usually the round that overflowed), every retained result
/// still owns its call, and nothing is deleted from the durable transcript —
/// the fold bounds the active surface, not the record.
#[test]
fn tool_evidence_keeps_its_class_across_a_fold() {
    let mut messages = vec![
        Message::text(Role::System, "sys"),
        Message::user_input("fix the failing test"),
    ];
    let call = |id: &str, name: &str| {
        Message::from_parts(
            Role::Assistant,
            vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.into(),
                    arguments: Default::default(),
                },
            }],
            None,
        )
    };
    let result = |id: &str, content: &str, is_error: bool| {
        Message::from_parts(
            Role::Tool,
            vec![ContentPart::ToolResult {
                result: ToolResultContent {
                    call_id: ToolCallId::new(id),
                    content: content.to_string(),
                    is_error,
                },
            }],
            None,
        )
    };
    messages.push(call("c1", "run_command"));
    messages.push(result(
        "c1",
        "error[E0308]: mismatched types --> src/lib.rs:12:5",
        true,
    ));
    for index in 0..10 {
        messages.push(call(&format!("f{index}"), "read_file"));
        messages.push(result(&format!("f{index}"), "bulk read output", false));
    }
    messages.push(call("c2", "run_command"));
    messages.push(result("c2", "test result: ok. 12 passed", false));

    let folded = compact_messages(
        &messages,
        4,
        0,
        Some("briefing"),
        Some("fix the failing test"),
    );
    let surfaced: Vec<(String, bool)> = folded
        .iter()
        .flat_map(|message| {
            message.content.iter().filter_map(|part| match part {
                ContentPart::ToolResult { result } => {
                    Some((result.content.clone(), result.is_error))
                }
                _ => None,
            })
        })
        .collect();
    assert!(
        surfaced
            .iter()
            .any(|(text, is_error)| text.contains("12 passed") && !is_error),
        "the newest observation is retained verbatim: {surfaced:?}"
    );
    assert!(
        surfaced.iter().all(|(_, is_error)| !is_error),
        "the retained working set is the newest, successful round"
    );
    // The elided failure is not deleted, it is out of the active surface: the
    // durable transcript is what the runtime loaded, unchanged.
    assert!(
        messages.iter().any(|message| message.content.iter().any(
            |part| matches!(part, ContentPart::ToolResult { result } if result.content.contains("E0308") && result.is_error)
        )),
        "a failure stays recorded in the transcript the fold copied"
    );
    // Its content is exactly what the summarizer is handed, so the briefing can
    // carry the failure forward; and the fallback says so when it cannot.
    let fallback = compact_messages(&messages, 4, 0, None, None);
    assert!(
        fallback.iter().any(|message| message
            .text_content()
            .contains("Summarization was unavailable")),
        "a fold without a briefing says the detail is unavailable instead of implying it survived"
    );
    leveler_model::validate_tool_exchange(&folded).expect("the folded surface is a valid exchange");
    leveler_model::validate_tool_exchange(&fallback).expect("the fallback is a valid exchange");
}

/// The pressure rule: fold only when the projected request is over the
/// threshold, and the projected size really falls afterwards. All figures come
/// from the production estimator — no provider token usage is involved.
#[test]
fn the_projection_falls_only_after_a_fold_over_the_threshold() {
    let tools = tools();
    let messages = transcript(10, 400, 100);
    let threshold = 1_000u64;

    let before = replayed_reasoning_tokens(&messages, &tools, ReasoningRetention::All);
    assert!(
        before > threshold,
        "the fixture is over the threshold: {before} > {threshold}"
    );

    let folded = compact_messages(
        &messages,
        4,
        threshold / 2,
        Some("briefing"),
        Some("fix the bug"),
    );
    let after = estimate_tokens(&folded);
    assert!(
        after < before,
        "the fold released tokens: {before} -> {after}"
    );
    let folded_projection = replayed_reasoning_tokens(&folded, &tools, ReasoningRetention::All);
    assert!(
        folded_projection < before,
        "the projected request really shrank: {before} -> {folded_projection}"
    );

    // The same transcript under the threshold keeps every row: below the
    // threshold nothing is elided at all (only the host objective pin, which
    // compact_messages ensures independently, may be added).
    let small = transcript(1, 40, 20);
    assert!(estimate_tokens(&small) < threshold);
    let unfolded = compact_messages(&small, 4, 0, Some("briefing"), Some("fix the bug"));
    assert!(
        unfolded.len() <= small.len() + 1,
        "no row is elided under the threshold: {} -> {}",
        small.len(),
        unfolded.len()
    );
    for message in &small {
        assert!(
            unfolded.contains(message),
            "every row survives: {message:?}"
        );
    }
    assert!(
        !unfolded
            .iter()
            .any(|message| message.text_content().contains("was compacted")),
        "no briefing is placed when nothing was folded"
    );
}

/// Contract-1: on a raw-replay route, a retained assistant turn's reasoning
/// cannot be stripped, truncated or rewritten by the requested retention arm.
#[test]
fn contract_one_raw_replay_retained_reasoning_is_exact() {
    let tools = tools();
    let messages = transcript(4, 200, 40);
    let contract = route();
    assert!(
        contract.requires_exact_retained_reasoning(true),
        "the raw-field contract requires exact retained reasoning"
    );

    for arm in [
        ReasoningRetention::None,
        ReasoningRetention::LastTurns(1),
        ReasoningRetention::All,
    ] {
        let replayed = replayed_reasoning(&messages, &tools, contract, arm);
        assert_eq!(
            replayed.len(),
            4,
            "every retained turn replays its reasoning under {arm:?}"
        );
        for (index, text) in replayed.iter().enumerate() {
            assert!(
                text.starts_with(&format!("thinking-{index} ")),
                "reasoning is verbatim, not reordered or rewritten: {text}"
            );
        }
    }

    // The only legal way to stop replaying a turn's reasoning is to remove the
    // whole turn from the active surface.
    let folded = compact_messages(&messages, 2, 0, Some("briefing"), None);
    let replayed_after = replayed_reasoning(&folded, &tools, contract, ReasoningRetention::All);
    assert!(
        replayed_after.len() < 4,
        "the fold released whole rounds: {} replayed",
        replayed_after.len()
    );
    assert!(
        replayed_after
            .iter()
            .all(|text| text.starts_with("thinking-")),
        "surviving reasoning is still verbatim: {replayed_after:?}"
    );
}

/// Contract-2: an authenticated/opaque block the route authenticates survives a
/// retained turn intact, including its signature; a route that does not
/// authenticate it neither keeps it nor re-spells it as plain reasoning.
#[test]
fn contract_two_signed_block_stays_intact() {
    let call_id = ToolCallId::new("c1");
    let messages = vec![
        Message::user_input("go"),
        Message::from_parts(
            Role::Assistant,
            vec![
                ContentPart::SignedReasoning {
                    text: "signed thought".into(),
                    signature: "sig-xyz".into(),
                },
                ContentPart::ToolCall {
                    call: ToolCall {
                        id: call_id.clone(),
                        name: "read_file".into(),
                        arguments: Default::default(),
                    },
                },
            ],
            None,
        ),
    ];
    let signed = RequestProjection::project(
        &messages,
        &tools(),
        ReasoningReplayContract::signed_block(),
        ReasoningRetention::None,
    );
    let kept = &signed.messages()[1].content;
    assert!(
        kept.iter().any(|part| matches!(
            part,
            ContentPart::SignedReasoning { signature, .. } if signature == "sig-xyz"
        )),
        "the signed block is retained with its signature: {kept:?}"
    );
    assert!(
        signed.summary().protocol_protected_turns >= 1,
        "the requested arm could not drop it: {:?}",
        signed.summary()
    );

    // A route with no channel for signed blocks keeps none and invents no
    // plain-reasoning substitute.
    let plain = RequestProjection::project(
        &messages,
        &tools(),
        ReasoningReplayContract::NONE,
        ReasoningRetention::All,
    );
    assert!(
        !plain.messages()[1].content.iter().any(|part| matches!(
            part,
            ContentPart::SignedReasoning { .. } | ContentPart::Reasoning { .. }
        )),
        "a route without the channel carries neither block nor substitute"
    );
}

/// Context-1 + Context-6: any cut the fold makes keeps each tool exchange whole
/// and keeps the route contract valid, no matter how aggressively the recent
/// budget is trimmed.
#[test]
fn context_cuts_keep_exchanges_whole_and_the_contract_valid() {
    let contract = route();
    let tools = tools();
    let messages = transcript(12, 120, 40);

    // A range of aggressive-to-conservative recent budgets, including zero
    // (keep only round boundaries). The tool-call/result pairing must survive
    // every one of them.
    for (keep_messages, keep_tokens) in [(0usize, 0u64), (2, 0), (3, 60), (6, 400), (12, 0)] {
        let folded = compact_messages(
            &messages,
            keep_messages,
            keep_tokens,
            Some("briefing"),
            None,
        );
        let projected =
            RequestProjection::project(&folded, &tools, contract, ReasoningRetention::All);
        // Every retained assistant reasoning is exact under the raw contract.
        for message in projected.messages() {
            if let leveler_model::ProjectedReasoning::Captured(text) = &message.reasoning {
                assert!(
                    text.starts_with("thinking-"),
                    "retained reasoning is exact under keep=({keep_messages},{keep_tokens}): {text}"
                );
            }
        }
        // Every retained tool result is preceded by its assistant call.
        let mut open: Vec<String> = Vec::new();
        for message in projected.messages() {
            for part in &message.content {
                match part {
                    ContentPart::ToolCall { call } => open.push(call.id.to_string()),
                    ContentPart::ToolResult { result } => {
                        let id = result.call_id.to_string();
                        assert!(
                            open.contains(&id),
                            "a retained result keeps its call under keep=({keep_messages},{keep_tokens})"
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Context-5: repeated folds converge on ONE active briefing, keep its
/// authority AdvisoryContext, and never leave the newest tool exchange half
/// elided.
#[test]
fn repeated_folds_keep_one_advisory_summary_and_a_whole_exchange() {
    let tools = tools();
    let mut messages = transcript(8, 160, 60);
    for round in 0..6 {
        // New work arrives between folds, as it does in a long session.
        messages.extend(thought_round(
            100 + round,
            &format!("later thinking {round} {}", "t".repeat(120)),
            &format!("later result {round} {}", "r".repeat(60)),
        ));
        let folded = compact_messages(
            &messages,
            6,
            500,
            Some("briefing: still working on the fix"),
            Some("fix the bug"),
        );
        // At most one live briefing, and never a second one stacked on top.
        let summaries = folded
            .iter()
            .filter(|m| {
                m.text_content()
                    .contains(leveler_model::COMPACTION_BREADCRUMB_MARKER)
            })
            .count();
        assert!(
            summaries <= 1,
            "round {round}: one active summary, got {summaries}"
        );
        let projection =
            RequestProjection::project(&folded, &tools, route(), ReasoningRetention::All);
        for class in projection.transcript_authority() {
            if class.source == PromptSource::CompactionSummary {
                assert_eq!(
                    class.authority,
                    PromptAuthority::AdvisoryContext,
                    "round {round}: the briefing stays advisory"
                );
            }
        }
        // Every retained result still has the call that opened it.
        let mut open: Vec<String> = Vec::new();
        for message in projection.messages() {
            for part in &message.content {
                match part {
                    ContentPart::ToolCall { call } => open.push(call.id.to_string()),
                    ContentPart::ToolResult { result } => {
                        let id = result.call_id.to_string();
                        assert!(
                            open.contains(&id),
                            "round {round}: a retained result keeps its call"
                        );
                    }
                    _ => {}
                }
            }
        }
        messages = folded;
    }
}
