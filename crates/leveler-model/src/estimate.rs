//! The ONE token estimator.
//!
//! Every model-visible cost figure CodeLeveler derives without a provider
//! tokenizer comes from here: the compaction pressure estimate, the agent's
//! token-budget fallback, and the context-accounting breakdown. One formula in
//! one place is what lets two of those numbers be compared at all — before
//! this module there were three copies, and the copies disagreed about
//! [`ContentPart::Reasoning`], so the accounting counted history the pressure
//! estimator could not see.
//!
//! # What is counted
//!
//! * text and reasoning at prose density (~4 ASCII bytes, ~3 bytes for
//!   non-ASCII, per token);
//! * tool calls, tool results and tool schemas at tool density (~2.5 bytes per
//!   token), measured against DeepSeek-reported usage (C5-S2);
//! * one flat cost per image, so a vision turn is not free.
//!
//! # What is deliberately NOT counted
//!
//! Per-message role framing and JSON envelope structure. No provider this
//! harness speaks to has been measured for those, and inventing a constant
//! would only make the calibration fixtures pass against a number nobody
//! observed. The estimator is a lower bound on the provider's prompt count;
//! the provider's own `prompt_tokens` is the authority once a request was
//! made.

use crate::message::{ContentPart, Message, ToolDefinition};

/// A conservative flat cost (in ASCII byte-equivalents, ÷4 below) for one
/// image, so a vision turn is not counted as ~free. Real vision billing is
/// tile-based and model-specific; ~1000 tokens/image is a safe floor.
pub const IMAGE_BYTE_EQUIV: u64 = 4096;

/// One accumulator of estimator byte buckets.
///
/// Buckets are accumulated in bytes and divided exactly once by
/// [`Self::tokens`], so a per-slice total (context accounting) and a
/// whole-transcript total (compaction pressure) are the same arithmetic on the
/// same bytes — the reason the two numbers cannot drift.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TokenEstimate {
    /// ASCII bytes of prose-shaped content (text, reasoning).
    ascii_text: u64,
    /// ASCII bytes of tool-shaped content (calls, results, schemas).
    ascii_tool: u64,
    /// Non-ASCII bytes of either shape; ~3 bytes per token.
    wide: u64,
    /// Pre-divided flat costs (images).
    flat: u64,
}

impl TokenEstimate {
    /// Empty accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add prose-shaped content (assistant text, reasoning, system prompts).
    pub fn add_text(&mut self, text: &str) {
        let (ascii, wide) = split(text);
        self.ascii_text += ascii;
        self.wide += wide;
    }

    /// Add tool-shaped content (call names/arguments, tool results, schemas).
    pub fn add_tool(&mut self, text: &str) {
        let (ascii, wide) = split(text);
        self.ascii_tool += ascii;
        self.wide += wide;
    }

    /// Estimate opaque ASCII replay metadata without retaining another copy.
    pub fn add_opaque_bytes(&mut self, bytes: usize) {
        self.ascii_text = self.ascii_text.saturating_add(bytes as u64);
    }

    /// Add one image at the conservative flat floor.
    pub fn add_image(&mut self) {
        self.flat += IMAGE_BYTE_EQUIV / 4;
    }

    /// Fold one content part. Role framing is not priced (see the module
    /// docs); reasoning is prose and prices as text.
    pub fn add_part(&mut self, part: &ContentPart) {
        match part {
            ContentPart::Text { text } | ContentPart::Reasoning { text } => {
                self.add_text(text);
            }
            ContentPart::SignedReasoning { text, signature } => {
                self.add_text(text);
                self.add_text(signature);
            }
            ContentPart::RedactedReasoning { data } => self.add_text(data),
            ContentPart::ToolCall { call } => {
                self.add_tool(&call.name);
                self.add_tool(&call.arguments.to_string());
            }
            ContentPart::ToolResult { result } => self.add_tool(&result.content),
            ContentPart::Image { .. } => self.add_image(),
        }
    }

    /// Fold one message's content parts. Role framing is not priced (see the
    /// module docs).
    pub fn add_message(&mut self, message: &Message) {
        for part in &message.content {
            self.add_part(part);
        }
    }

    /// Fold one tool definition the way the wire spells it: name, description
    /// and input schema.
    pub fn add_tool_definition(&mut self, tool: &ToolDefinition) {
        self.add_text(&tool.description);
        self.add_tool(&tool.name);
        self.add_tool(&tool.input_schema.to_string());
    }

    /// The token estimate for everything accumulated so far.
    pub fn tokens(&self) -> u64 {
        self.ascii_text / 4 + self.ascii_tool * 2 / 5 + self.wide / 3 + self.flat
    }
}

/// (ascii bytes, wide bytes) split. Non-ASCII (CJK, …) spends ~1 token per
/// character (~3 UTF-8 bytes), so those bytes are weighted at 3 bytes/token
/// rather than the flat ÷4 used for ASCII prose.
fn split(s: &str) -> (u64, u64) {
    let ascii = s.bytes().filter(u8::is_ascii).count() as u64;
    (ascii, s.len() as u64 - ascii)
}

/// Estimated tokens of one prose block, using the same accumulator as requests.
pub fn estimate_text(text: &str) -> u64 {
    let mut estimate = TokenEstimate::new();
    estimate.add_text(text);
    estimate.tokens()
}

/// Estimated model-visible tokens of a transcript's content parts.
///
/// This is the fallback path for providers/gateways that report no usage, and
/// the composition estimate behind compaction pressure. It is *not* a
/// replacement for [`crate::TokenUsage::input_tokens`]: once a request was
/// made, the provider's own number is the authority.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    let mut estimate = TokenEstimate::new();
    for message in messages {
        estimate.add_message(message);
    }
    estimate.tokens()
}

/// Estimated model-visible tokens of one message.
pub fn estimate_message(message: &Message) -> u64 {
    let mut estimate = TokenEstimate::new();
    estimate.add_message(message);
    estimate.tokens()
}

/// Estimated model-visible tokens of the tool schemas a request exposes.
pub fn estimate_tool_definitions(tools: &[ToolDefinition]) -> u64 {
    let mut estimate = TokenEstimate::new();
    for tool in tools {
        estimate.add_tool_definition(tool);
    }
    estimate.tokens()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Role, ToolCall};
    use leveler_core::ToolCallId;

    #[test]
    fn reasoning_is_counted_as_prose() {
        // Reasoning is assistant content the provider request carries; an
        // estimator that skips it under-reports the request by the whole
        // reasoning history.
        let with = vec![Message {
            origin: None,
            role: Role::Assistant,
            content: vec![
                ContentPart::Reasoning {
                    text: "a".repeat(4000),
                },
                ContentPart::Text {
                    text: "b".repeat(4000),
                },
            ],
        }];
        let without = vec![Message::text(Role::Assistant, "b".repeat(4000))];
        assert_eq!(estimate_tokens(&with) - estimate_tokens(&without), 1000);
    }

    #[test]
    fn tool_payloads_are_weighted_denser_than_prose() {
        let body = "{\"path\":\"src/lib.rs\",\"exit\":0}".repeat(100);
        let as_text = estimate_tokens(&[Message::text(Role::User, &body)]);
        let as_tool = estimate_tokens(&[Message {
            origin: None,
            role: Role::User,
            content: vec![ContentPart::ToolResult {
                result: crate::message::ToolResultContent {
                    call_id: ToolCallId::new("c"),
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

    #[test]
    fn tool_call_arguments_are_tool_shaped() {
        let message = Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new("c"),
                    name: "grep".into(),
                    arguments: serde_json::json!({"pattern": "x".repeat(100)}),
                },
            }],
        };
        let mut split_estimate = TokenEstimate::new();
        split_estimate.add_message(&message);
        assert_eq!(estimate_message(&message), split_estimate.tokens());
        assert_eq!(
            estimate_tokens(std::slice::from_ref(&message)),
            split_estimate.tokens()
        );
    }

    #[test]
    fn tool_definitions_are_counted() {
        let tool = ToolDefinition {
            name: "grep".into(),
            description: "Search files".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        assert!(estimate_tool_definitions(std::slice::from_ref(&tool)) > 0);
    }

    #[test]
    fn images_are_not_free() {
        let with_image = vec![Message {
            origin: None,
            role: Role::User,
            content: vec![ContentPart::Image {
                source: crate::message::ImageSource::Url {
                    url: "https://x/y.png".to_string(),
                },
            }],
        }];
        assert!(estimate_tokens(&with_image) >= 256);
    }

    #[test]
    fn slice_and_whole_transcript_agree() {
        // The property accounting relies on: pricing each part into its own
        // bucket and summing must equal pricing the whole transcript at once.
        let messages = vec![
            Message::text(Role::System, "rules ".repeat(100)),
            Message {
                origin: None,
                role: Role::Assistant,
                content: vec![
                    ContentPart::Reasoning {
                        text: "想".repeat(50),
                    },
                    ContentPart::ToolCall {
                        call: ToolCall {
                            id: ToolCallId::new("c"),
                            name: "read".into(),
                            arguments: serde_json::json!({"path": "a.rs"}),
                        },
                    },
                ],
            },
        ];
        let mut per_message = 0;
        for message in &messages {
            per_message += estimate_message(message);
        }
        let whole = estimate_tokens(&messages);
        // Per-message rounding (÷4 etc.) can differ by at most one token per
        // message; the accumulator form used by accounting divides once.
        assert!(
            whole >= per_message,
            "whole={whole} per_message={per_message}"
        );
        assert!(whole - per_message <= messages.len() as u64);
    }
}
