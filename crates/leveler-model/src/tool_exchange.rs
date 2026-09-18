//! The provider-bound tool-exchange invariant.
//!
//! A tool exchange is one assistant message carrying `ToolCall` parts followed
//! directly by the `Role::Tool` message(s) carrying a `ToolResult` for each of
//! those calls. The runtime appends a round as exactly that pair. Every wire
//! protocol requires it: Chat Completions rejects a `role: tool` message that
//! does not answer the preceding `assistant.tool_calls`, and Anthropic
//! rejects a `tool_result` without its `tool_use`.
//!
//! [`validate_tool_exchange`] checks the unified sequence, before any adapter
//! sees it. It never repairs: a sequence that breaks the invariant was built
//! wrong somewhere upstream, and the caller must fail instead of sending it.

use leveler_core::ToolCallId;

use crate::message::{ContentPart, Message, Role};

/// How a message sequence breaks the tool-exchange invariant. `index` is the
/// position in the validated slice.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolExchangeViolation {
    /// A tool result with no assistant `tool_calls` directly before it.
    #[error(
        "orphan tool result: message {index} answers call `{call_id}` but no assistant tool_calls precede it"
    )]
    OrphanResult { index: usize, call_id: ToolCallId },
    /// A tool result naming a call the preceding assistant never issued.
    #[error(
        "tool result at message {index} answers call `{call_id}`, which the preceding assistant tool_calls do not contain"
    )]
    UnknownCallId { index: usize, call_id: ToolCallId },
    /// A second result for a call that was already answered.
    #[error("duplicate tool result at message {index} for call `{call_id}`")]
    DuplicateResult { index: usize, call_id: ToolCallId },
    /// An assistant tool call with no result before the exchange closed.
    #[error("assistant tool call `{call_id}` at message {index} has no tool result")]
    UnansweredCall { index: usize, call_id: ToolCallId },
}

/// Check that every tool result answers a call of the assistant message that
/// opened its exchange, exactly once, and that every call is answered before
/// the next non-tool message (or the end of the sequence).
pub fn validate_tool_exchange(messages: &[Message]) -> Result<(), ToolExchangeViolation> {
    // The open exchange: the assistant's index and each call with whether it
    // has been answered yet.
    let mut open: Option<(usize, Vec<(&ToolCallId, bool)>)> = None;
    for (index, message) in messages.iter().enumerate() {
        if message.role == Role::Tool {
            for part in &message.content {
                let ContentPart::ToolResult { result } = part else {
                    continue;
                };
                let call_id = &result.call_id;
                let Some((_, calls)) = open.as_mut() else {
                    return Err(ToolExchangeViolation::OrphanResult {
                        index,
                        call_id: call_id.clone(),
                    });
                };
                match calls.iter_mut().find(|(id, _)| *id == call_id) {
                    None => {
                        return Err(ToolExchangeViolation::UnknownCallId {
                            index,
                            call_id: call_id.clone(),
                        });
                    }
                    Some((_, true)) => {
                        return Err(ToolExchangeViolation::DuplicateResult {
                            index,
                            call_id: call_id.clone(),
                        });
                    }
                    Some((_, answered)) => *answered = true,
                }
            }
            continue;
        }
        close_exchange(open.take())?;
        if message.role == Role::Assistant {
            let calls: Vec<(&ToolCallId, bool)> = message
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::ToolCall { call } => Some((&call.id, false)),
                    _ => None,
                })
                .collect();
            if !calls.is_empty() {
                open = Some((index, calls));
            }
        }
    }
    close_exchange(open)
}

fn close_exchange(
    open: Option<(usize, Vec<(&ToolCallId, bool)>)>,
) -> Result<(), ToolExchangeViolation> {
    let Some((index, calls)) = open else {
        return Ok(());
    };
    match calls.into_iter().find(|(_, answered)| !answered) {
        Some((call_id, _)) => Err(ToolExchangeViolation::UnansweredCall {
            index,
            call_id: call_id.clone(),
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ToolCall, ToolResultContent};

    fn call(ids: &[&str]) -> Message {
        Message {
            role: Role::Assistant,
            content: ids
                .iter()
                .map(|id| ContentPart::ToolCall {
                    call: ToolCall {
                        id: ToolCallId::new(*id),
                        name: "read_file".into(),
                        arguments: serde_json::json!({}),
                    },
                })
                .collect(),
        }
    }

    fn results(ids: &[&str]) -> Message {
        Message {
            role: Role::Tool,
            content: ids
                .iter()
                .map(|id| ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: ToolCallId::new(*id),
                        content: "ok".into(),
                        is_error: false,
                    },
                })
                .collect(),
        }
    }

    fn text(role: Role, t: &str) -> Message {
        Message::text(role, t)
    }

    #[test]
    fn a_single_tool_round_is_valid() {
        let msgs = [
            text(Role::System, "sys"),
            text(Role::User, "task"),
            call(&["a"]),
            results(&["a"]),
            text(Role::Assistant, "done"),
        ];
        assert_eq!(validate_tool_exchange(&msgs), Ok(()));
    }

    #[test]
    fn a_multi_call_round_is_valid_as_one_or_several_tool_messages() {
        let grouped = [
            text(Role::User, "t"),
            call(&["a", "b"]),
            results(&["a", "b"]),
        ];
        assert_eq!(validate_tool_exchange(&grouped), Ok(()));
        let split = [
            text(Role::User, "t"),
            call(&["a", "b"]),
            results(&["b"]),
            results(&["a"]),
        ];
        assert_eq!(validate_tool_exchange(&split), Ok(()));
    }

    #[test]
    fn consecutive_tool_rounds_are_valid() {
        let msgs = [
            text(Role::User, "t"),
            call(&["a"]),
            results(&["a"]),
            call(&["b", "c"]),
            results(&["b", "c"]),
            text(Role::Assistant, "done"),
            text(Role::User, "next"),
        ];
        assert_eq!(validate_tool_exchange(&msgs), Ok(()));
    }

    #[test]
    fn a_result_without_its_assistant_call_is_an_orphan() {
        let msgs = [text(Role::User, "t"), results(&["a"])];
        assert_eq!(
            validate_tool_exchange(&msgs),
            Err(ToolExchangeViolation::OrphanResult {
                index: 1,
                call_id: ToolCallId::new("a"),
            })
        );
        // A plain assistant message does not open an exchange.
        let after_text = [text(Role::Assistant, "done"), results(&["a"])];
        assert!(matches!(
            validate_tool_exchange(&after_text),
            Err(ToolExchangeViolation::OrphanResult { index: 1, .. })
        ));
    }

    #[test]
    fn a_result_after_a_closed_exchange_is_an_orphan() {
        // The exchange closed at the user message; a later result has no owner.
        let msgs = [
            call(&["a"]),
            results(&["a"]),
            text(Role::User, "u"),
            results(&["a"]),
        ];
        assert!(matches!(
            validate_tool_exchange(&msgs),
            Err(ToolExchangeViolation::OrphanResult { index: 3, .. })
        ));
    }

    #[test]
    fn a_mismatched_call_id_is_rejected() {
        let msgs = [call(&["a"]), results(&["x"])];
        assert_eq!(
            validate_tool_exchange(&msgs),
            Err(ToolExchangeViolation::UnknownCallId {
                index: 1,
                call_id: ToolCallId::new("x"),
            })
        );
    }

    #[test]
    fn a_duplicate_result_is_rejected() {
        let msgs = [call(&["a"]), results(&["a"]), results(&["a"])];
        assert!(matches!(
            validate_tool_exchange(&msgs),
            Err(ToolExchangeViolation::DuplicateResult { index: 2, .. })
        ));
    }

    #[test]
    fn an_unanswered_call_is_rejected_mid_sequence_and_at_the_end() {
        let mid = [call(&["a", "b"]), results(&["a"]), text(Role::User, "u")];
        assert_eq!(
            validate_tool_exchange(&mid),
            Err(ToolExchangeViolation::UnansweredCall {
                index: 0,
                call_id: ToolCallId::new("b"),
            })
        );
        let end = [text(Role::User, "u"), call(&["a"])];
        assert!(matches!(
            validate_tool_exchange(&end),
            Err(ToolExchangeViolation::UnansweredCall { index: 1, .. })
        ));
    }
}
