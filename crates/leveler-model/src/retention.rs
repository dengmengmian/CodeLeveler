//! Historical-reasoning retention: the REQUESTED policy for how much
//! already-produced assistant reasoning is re-sent to the provider.
//!
//! This module owns one thing: which reasoning-bearing assistant turns an arm
//! keeps. It does not rewrite messages, and it does not know about protocols.
//! [`crate::RequestProjection`] is the owner of the provider-visible request
//! and applies the protocol's requirements on top of whatever this policy
//! requests — reporting the difference instead of absorbing it.
//!
//! # Why this is not a per-turn lever
//!
//! A route that replays captured reasoning requires the *full* reasoning of
//! every assistant turn still in the active history — not only the turns that
//! carried a tool call. DeepSeek documents that a tool-bearing request must
//! pass back the reasoning of all previous turns and rejects a request that
//! omits the field (HTTP 400: "The `reasoning_content` in the thinking mode
//! must be passed back to the API"); an experiment that requests `None`
//! therefore cannot get `None` while such a turn is still present — and must
//! not be *recorded* as if it had. The projection counts those turns in
//! [`crate::ReasoningProjectionSummary::protocol_protected_turns`].
//!
//! # The lever is the lifecycle
//!
//! The mechanical way to stop paying for old reasoning is for the *context
//! lifecycle* to fold whole old rounds out of the active surface, uniformly
//! with the tool calls and results they contained. A turn that is no longer in
//! the active surface is no longer replayed. Providers that expose their own
//! history-reasoning control (for example GLM's `clear_thinking`) are the only
//! ones where a per-turn choice is the provider's to make.
//!
//! The durable transcript is never touched by this policy: the projection
//! changes what a provider request carries, and nothing else.

use serde::{Deserialize, Serialize};

use crate::message::{ContentPart, Message, Role};

/// How much historical assistant reasoning one request carries.
///
/// This is the REQUESTED arm. The protocol-safe effective view is produced by
/// [`crate::RequestProjection::project`], which reports any difference.
///
/// [`ReasoningRetention::All`] is the production default and is byte-identical
/// to carrying every reasoning block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReasoningRetention {
    /// Every historical reasoning block is carried.
    All,
    /// Only the `n` most recent reasoning-bearing assistant turns keep their
    /// reasoning; earlier ones keep text and tool calls.
    LastTurns(usize),
    /// No historical reasoning is carried.
    None,
}

impl Default for ReasoningRetention {
    fn default() -> Self {
        Self::All
    }
}

impl ReasoningRetention {
    /// The indices of the assistant turns whose reasoning this arm carries.
    ///
    /// The arm counts **reasoning-bearing assistant turns** — including turns
    /// that carried tool calls — never raw messages: otherwise the tool results
    /// interleaved between assistant turns would silently shrink the window.
    /// Whether the provider then accepts the result is the projection's
    /// question, not this one.
    pub fn carried_positions(&self, messages: &[Message]) -> Vec<usize> {
        let positions: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::Assistant && carries_reasoning(m))
            .map(|(i, _)| i)
            .collect();
        match self {
            Self::All => positions,
            Self::LastTurns(keep) => positions[positions.len().saturating_sub(*keep)..].to_vec(),
            Self::None => Vec::new(),
        }
    }

    /// A stable, human-readable arm name for experiment provenance and tests.
    pub fn arm_name(&self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::LastTurns(n) => format!("last_{n}"),
            Self::None => "none".to_string(),
        }
    }
}

/// Whether this message carries captured reasoning.
pub fn carries_reasoning(message: &Message) -> bool {
    message.content.iter().any(|part| {
        matches!(
            part,
            ContentPart::Reasoning { .. }
                | ContentPart::SignedReasoning { .. }
                | ContentPart::RedactedReasoning { .. }
        )
    })
}

/// Whether this message requests a tool — i.e. whether its reasoning is part of
/// a tool exchange a provider validates.
pub fn carries_tool_call(message: &Message) -> bool {
    message
        .content
        .iter()
        .any(|part| matches!(part, ContentPart::ToolCall { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(reasoning: &str, text: &str) -> Message {
        let mut content = Vec::new();
        if !reasoning.is_empty() {
            content.push(ContentPart::Reasoning {
                text: reasoning.to_string(),
            });
        }
        content.push(ContentPart::Text {
            text: text.to_string(),
        });
        Message {
            origin: None,
            role: Role::Assistant,
            content,
        }
    }

    fn tool_turn(reasoning: &str) -> Message {
        Message {
            origin: None,
            role: Role::Assistant,
            content: vec![
                ContentPart::Reasoning {
                    text: reasoning.to_string(),
                },
                ContentPart::ToolCall {
                    call: crate::message::ToolCall {
                        id: leveler_core::ToolCallId::from("c1"),
                        name: "read".into(),
                        arguments: serde_json::json!({}),
                    },
                },
            ],
        }
    }

    fn tool_result(text: &str) -> Message {
        Message {
            origin: None,
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                result: crate::message::ToolResultContent {
                    call_id: leveler_core::ToolCallId::from("c1"),
                    content: text.to_string(),
                    is_error: false,
                },
            }],
        }
    }

    fn kept(policy: ReasoningRetention, messages: &[Message]) -> Vec<usize> {
        policy.carried_positions(messages)
    }

    fn five_turns() -> Vec<Message> {
        // Five reasoning-bearing assistant turns with a tool result between
        // each pair, so the window can only be counted over assistant turns.
        let mut messages = Vec::new();
        for i in 1..=5 {
            messages.push(assistant(&format!("r{i}"), &format!("t{i}")));
            messages.push(tool_result(&format!("o{i}")));
        }
        messages
    }

    #[test]
    fn all_keeps_every_reasoning_bearing_turn() {
        assert_eq!(kept(ReasoningRetention::All, &five_turns()).len(), 5);
    }

    #[test]
    fn last_three_keeps_only_the_three_most_recent_turns() {
        let messages = five_turns();
        let positions = kept(ReasoningRetention::LastTurns(3), &messages);
        let texts: Vec<&str> = positions
            .iter()
            .map(|i| match &messages[*i].content[0] {
                ContentPart::Reasoning { text } => text.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(texts, vec!["r3", "r4", "r5"]);
    }

    #[test]
    fn none_keeps_nothing() {
        assert!(kept(ReasoningRetention::None, &five_turns()).is_empty());
    }

    /// The arm counts reasoning-bearing turns, so a text-only assistant turn
    /// does not consume a window slot.
    #[test]
    fn assistant_messages_without_reasoning_do_not_consume_the_window() {
        let mut messages = vec![assistant("", "plain"), tool_result("x")];
        messages.extend(five_turns());
        let positions = kept(ReasoningRetention::LastTurns(3), &messages);
        assert_eq!(positions.len(), 3);
        assert!(
            !positions.contains(&0),
            "the text-only turn is not in the window"
        );
    }

    /// A tool-call turn is a reasoning-bearing turn for the ARM's purposes; the
    /// projection is what refuses to drop it.
    #[test]
    fn a_tool_call_turn_counts_toward_the_requested_window() {
        let messages = vec![
            tool_turn("r-tool"),
            tool_result("o"),
            assistant("r1", "t1"),
            assistant("r2", "t2"),
        ];
        assert_eq!(
            kept(ReasoningRetention::LastTurns(2), &messages),
            vec![2, 3]
        );
    }

    #[test]
    fn a_window_wider_than_the_transcript_keeps_everything() {
        assert_eq!(
            kept(ReasoningRetention::LastTurns(10), &five_turns()).len(),
            5
        );
    }

    #[test]
    fn arm_names_are_stable() {
        assert_eq!(ReasoningRetention::All.arm_name(), "all");
        assert_eq!(ReasoningRetention::LastTurns(3).arm_name(), "last_3");
        assert_eq!(ReasoningRetention::None.arm_name(), "none");
    }

    #[test]
    fn default_is_all() {
        assert_eq!(ReasoningRetention::default(), ReasoningRetention::All);
    }
}
