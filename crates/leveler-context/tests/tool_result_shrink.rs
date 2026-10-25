//! Hard-capacity recovery for a transcript that is over the window only
//! because a single tool result is large.
//!
//! A short task can have nothing to fold: `compaction_span` needs a middle
//! between the task anchor and the retained tail, and a transcript of five
//! messages has none. If one of those messages is a 48 KiB tool result, the
//! request is over the hard capacity while no fold can remove anything. The
//! recovery is deterministic: an already-externalized result keeps its
//! reference and gives up its preview, oldest first. Nothing is silently
//! dropped — the full content stays at the path the reference names.

use leveler_context::{compaction_span, shrink_externalized_results};
use leveler_core::ToolCallId;
use leveler_model::tool_output::saved_reference;
use leveler_model::{
    ContentPart, Message, Role, ToolCall, ToolResultContent, estimate_tokens,
};

const BIG: usize = 40_000;

fn call(id: &str) -> Message {
    Message::from_parts(
        Role::Assistant,
        vec![ContentPart::ToolCall {
            call: ToolCall {
                id: ToolCallId::new(id),
                name: "grep".into(),
                arguments: Default::default(),
            },
        }],
        None,
    )
}

fn saved_result(id: &str, path: &str) -> ContentPart {
    ContentPart::ToolResult {
        result: ToolResultContent {
            call_id: ToolCallId::new(id),
            content: format!(
                "{}\nHEAD\n{}\nTAIL",
                saved_reference(path, "deadbeef", BIG),
                "x".repeat(BIG)
            ),
            is_error: false,
        },
    }
}

fn plain_result(id: &str, content: &str) -> ContentPart {
    ContentPart::ToolResult {
        result: ToolResultContent {
            call_id: ToolCallId::new(id),
            content: content.to_string(),
            is_error: false,
        },
    }
}

fn results(parts: Vec<ContentPart>) -> Message {
    Message::from_parts(Role::Tool, parts, None)
}

fn result_text(message: &Message, index: usize) -> &str {
    match &message.content[index] {
        ContentPart::ToolResult { result } => &result.content,
        other => panic!("expected a tool result at {index}: {other:?}"),
    }
}

/// The shape T8 produced: system + task anchor + one tool exchange whose
/// result is huge. There is no foldable middle, so the fold is a no-op.
#[test]
fn a_tool_heavy_short_transcript_has_nothing_to_fold() {
    let messages = vec![
        Message::text(Role::System, "system prompt"),
        Message::text(Role::User, "find the window boundary"),
        call("call-1"),
        results(vec![saved_result("call-1", "/state/artifacts/big.txt")]),
    ];
    assert_eq!(
        compaction_span(&messages, 12, 0),
        None,
        "the fixture must reproduce the no-fold shape"
    );
}

/// Recovery reduces the OLDEST externalized result first and stops as soon as
/// the measured transcript fits. A newer result is the working set and stays.
#[test]
fn externalized_results_are_reduced_oldest_first_until_the_transcript_fits() {
    let messages = vec![
        Message::text(Role::System, "system prompt"),
        Message::text(Role::User, "find the window boundary"),
        call("call-1"),
        results(vec![saved_result("call-1", "/state/artifacts/old.txt")]),
        call("call-2"),
        results(vec![saved_result("call-2", "/state/artifacts/new.txt")]),
        Message::text(Role::User, "continue"),
    ];
    let before = estimate_tokens(&messages);
    let target = before - 1_000;

    let (reduced, count) = shrink_externalized_results(&messages, target, estimate_tokens);

    assert_eq!(count, 1, "one reduction was enough to reach the target");
    assert!(
        estimate_tokens(&reduced) <= target,
        "the recovered transcript must fit: {} > {target}",
        estimate_tokens(&reduced)
    );
    let old = result_text(&reduced[3], 0);
    let new = result_text(&reduced[5], 0);
    assert!(
        old.contains("/state/artifacts/old.txt") && !old.contains("xxxxxxxx"),
        "the oldest result keeps its reference and gives up the preview: {old}"
    );
    assert!(
        new.contains("xxxxxxxx"),
        "the newest result is the working set and must stay intact"
    );
    assert!(
        old.contains("full output is at the path above") || old.contains("full output"),
        "the reduction must say the content is still available: {old}"
    );
}

/// Reducing a result changes only its content. Message count, roles, call ids
/// and every non-tool message stay exactly as they were, so the provider's
/// tool-call/result pairing is untouched.
#[test]
fn reduction_preserves_the_tool_exchange_and_every_other_message() {
    let messages = vec![
        Message::text(Role::System, "system prompt"),
        Message::text(Role::User, "task"),
        call("call-1"),
        results(vec![
            saved_result("call-1", "/state/artifacts/a.txt"),
            plain_result("call-2", "small, unremarkable output"),
        ]),
    ];
    let before_roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
    let before_ids: Vec<String> = messages[3]
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult { result } => Some(result.call_id.as_str().to_string()),
            _ => None,
        })
        .collect();

    let (reduced, count) = shrink_externalized_results(&messages, 0, estimate_tokens);

    assert_eq!(count, 1);
    assert_eq!(reduced.len(), messages.len());
    assert_eq!(
        reduced.iter().map(|m| m.role).collect::<Vec<_>>(),
        before_roles
    );
    assert_eq!(reduced[0], messages[0]);
    assert_eq!(reduced[1], messages[1]);
    assert_eq!(reduced[2], messages[2]);
    let after_ids: Vec<String> = reduced[3]
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult { result } => Some(result.call_id.as_str().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(after_ids, before_ids, "pairing is identity, not content");
    assert_eq!(
        reduced[3].content[1],
        messages[3].content[1],
        "a result with no externalization marker is the only copy of its content"
    );
}

/// A transcript whose results carry no marker cannot be reduced: this path
/// has no right to invent a loss.
#[test]
fn results_without_an_externalization_marker_are_never_dropped() {
    let messages = vec![
        Message::text(Role::System, "system prompt"),
        Message::text(Role::User, "task"),
        call("call-1"),
        results(vec![plain_result("call-1", &"y".repeat(BIG))]),
    ];
    let (reduced, count) = shrink_externalized_results(&messages, 0, estimate_tokens);
    assert_eq!(count, 0);
    assert_eq!(reduced, messages);
}
