//! Streaming decode: turns Anthropic Messages SSE events into unified
//! [`ModelEvent`]s, reassembling `tool_use` inputs that arrive as fragmented
//! `input_json_delta` chunks. Mirrors the OpenAI assembler's contract: usage is
//! emitted before completion, and malformed tool JSON becomes an `Error` event —
//! never a guessed execution.

use std::collections::BTreeMap;

use leveler_core::ToolCallId;
use leveler_model::{FinishReason, ModelError, ModelErrorKind, ModelEvent, TokenUsage, ToolCall};

use super::wire::{
    BlockDelta, RespBlock, StreamContentBlockDelta, StreamContentBlockStart, StreamMessageDelta,
    StreamMessageStart,
};

/// Map an Anthropic `stop_reason` to the unified enum.
pub(super) fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        _ => FinishReason::Other,
    }
}

/// Per-content-block assembly state, keyed by the stream's block `index`.
#[derive(Debug, Default)]
struct BlockState {
    /// `Some((id, name))` for a `tool_use` block; `None` for text/thinking.
    tool: Option<(String, String)>,
    /// Accumulated `input_json_delta` fragments for a `tool_use` block.
    json: String,
}

/// Stateful assembler consuming decoded Anthropic events and emitting unified
/// events in order.
#[derive(Debug, Default)]
pub struct AnthropicStreamAssembler {
    blocks: BTreeMap<usize, BlockState>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    completed: bool,
}

impl AnthropicStreamAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one SSE event (its `event:` type and JSON `data`), returning the
    /// unified events it produced. A malformed payload yields a single decode
    /// `Error` event rather than aborting the stream.
    pub fn on_event(&mut self, event: &str, data: &str) -> Vec<ModelEvent> {
        match event {
            "message_start" => match serde_json::from_str::<StreamMessageStart>(data) {
                Ok(m) => {
                    if let Some(u) = m.message.usage {
                        self.input_tokens = u.input_tokens;
                        self.cache_read = u.cache_read_input_tokens;
                    }
                    Vec::new()
                }
                Err(e) => vec![decode_error(e)],
            },
            "content_block_start" => match serde_json::from_str::<StreamContentBlockStart>(data) {
                Ok(s) => match s.content_block {
                    RespBlock::ToolUse { id, name, .. } => {
                        self.blocks.insert(
                            s.index,
                            BlockState {
                                tool: Some((id.clone(), name.clone())),
                                json: String::new(),
                            },
                        );
                        vec![ModelEvent::ToolCallStarted {
                            index: s.index,
                            id: (!id.is_empty()).then(|| ToolCallId::new(id)),
                            name: (!name.is_empty()).then_some(name),
                        }]
                    }
                    _ => {
                        self.blocks.insert(s.index, BlockState::default());
                        Vec::new()
                    }
                },
                Err(e) => vec![decode_error(e)],
            },
            "content_block_delta" => match serde_json::from_str::<StreamContentBlockDelta>(data) {
                Ok(d) => match d.delta {
                    BlockDelta::TextDelta { text } if !text.is_empty() => {
                        vec![ModelEvent::TextDelta { delta: text }]
                    }
                    BlockDelta::ThinkingDelta { thinking } if !thinking.is_empty() => {
                        vec![ModelEvent::ReasoningDelta { delta: thinking }]
                    }
                    BlockDelta::InputJsonDelta { partial_json } => {
                        if let Some(b) = self.blocks.get_mut(&d.index) {
                            b.json.push_str(&partial_json);
                        }
                        if partial_json.is_empty() {
                            Vec::new()
                        } else {
                            vec![ModelEvent::ToolCallArgumentsDelta {
                                index: d.index,
                                delta: partial_json,
                            }]
                        }
                    }
                    _ => Vec::new(),
                },
                Err(e) => vec![decode_error(e)],
            },
            "content_block_stop" => Vec::new(),
            "message_delta" => match serde_json::from_str::<StreamMessageDelta>(data) {
                Ok(m) => {
                    if let Some(u) = m.usage {
                        self.output_tokens = u.output_tokens;
                    }
                    let reason = m
                        .delta
                        .stop_reason
                        .unwrap_or_else(|| "end_turn".to_string());
                    self.finalize(&reason)
                }
                Err(e) => vec![decode_error(e)],
            },
            "message_stop" => self.finalize("end_turn"),
            // `ping` and any unknown event carry nothing unified.
            _ => Vec::new(),
        }
    }

    /// Emit final usage, then completed tool calls (parsing their joined JSON),
    /// then the terminal `MessageCompleted`. Idempotent.
    fn finalize(&mut self, reason: &str) -> Vec<ModelEvent> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;
        let mut events = vec![ModelEvent::UsageUpdated {
            usage: TokenUsage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cached_input_tokens: self.cache_read,
            },
        }];

        for (index, block) in std::mem::take(&mut self.blocks) {
            let Some((id, name)) = block.tool else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let raw = if block.json.trim().is_empty() {
                "{}"
            } else {
                &block.json
            };
            // Strict JSON only — never repair-and-execute a guessed payload.
            match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(arguments) => {
                    let id = if id.is_empty() {
                        format!("call_{index}")
                    } else {
                        id
                    };
                    events.push(ModelEvent::ToolCallCompleted {
                        call: ToolCall {
                            id: ToolCallId::new(id),
                            name,
                            arguments,
                        },
                    });
                }
                Err(e) => {
                    let error = if reason == "max_tokens" {
                        ModelError::new(
                            ModelErrorKind::Truncated,
                            format!(
                                "输出被 max_tokens 上限截断,工具调用 `{name}` 的参数不完整（提高该模型的 max_output_tokens,或让改动分批更小）"
                            ),
                        )
                    } else {
                        ModelError::new(
                            ModelErrorKind::Decode,
                            format!(
                                "tool call `{name}` (index {index}) had invalid JSON arguments: {e}"
                            ),
                        )
                    };
                    events.push(ModelEvent::Error { error });
                }
            }
        }

        events.push(ModelEvent::MessageCompleted {
            finish_reason: map_stop_reason(reason),
        });
        events
    }

    /// Whether a terminal `MessageCompleted` has already been emitted.
    pub fn is_completed(&self) -> bool {
        self.completed
    }
}

fn decode_error(e: serde_json::Error) -> ModelEvent {
    ModelEvent::Error {
        error: ModelError::new(
            ModelErrorKind::Decode,
            format!("malformed stream event: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_deltas_are_emitted() {
        let mut a = AnthropicStreamAssembler::new();
        let evs = a.on_event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
        );
        assert_eq!(
            evs,
            vec![ModelEvent::TextDelta {
                delta: "Hel".into()
            }]
        );
    }

    #[test]
    fn tool_use_input_reassembled_across_deltas() {
        let mut a = AnthropicStreamAssembler::new();
        a.on_event(
            "message_start",
            r#"{"message":{"usage":{"input_tokens":10}}}"#,
        );
        a.on_event(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"grep","input":{}}}"#,
        );
        a.on_event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pat"}}"#,
        );
        a.on_event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"tern\":\"x\"}"}}"#,
        );
        a.on_event("content_block_stop", r#"{"index":0}"#);
        let evs = a.on_event(
            "message_delta",
            r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
        );
        let call = evs
            .iter()
            .find_map(|e| match e {
                ModelEvent::ToolCallCompleted { call } => Some(call),
                _ => None,
            })
            .expect("tool call should complete");
        assert_eq!(call.name, "grep");
        assert_eq!(call.id.as_str(), "tu_1");
        assert_eq!(call.arguments["pattern"], "x");
        assert!(evs.iter().any(|e| matches!(
            e,
            ModelEvent::MessageCompleted {
                finish_reason: FinishReason::ToolCalls
            }
        )));
    }

    #[test]
    fn usage_precedes_completion() {
        let mut a = AnthropicStreamAssembler::new();
        a.on_event(
            "message_start",
            r#"{"message":{"usage":{"input_tokens":85,"cache_read_input_tokens":64}}}"#,
        );
        let evs = a.on_event(
            "message_delta",
            r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":32}}"#,
        );
        let usage_at = evs
            .iter()
            .position(|e| matches!(e, ModelEvent::UsageUpdated { .. }))
            .expect("usage emitted");
        let done_at = evs
            .iter()
            .position(|e| matches!(e, ModelEvent::MessageCompleted { .. }))
            .expect("completion emitted");
        assert!(usage_at < done_at);
        let ModelEvent::UsageUpdated { usage } = &evs[usage_at] else {
            unreachable!()
        };
        assert_eq!(usage.input_tokens, 85);
        assert_eq!(usage.output_tokens, 32);
        assert_eq!(usage.cached_input_tokens, 64);
    }

    #[test]
    fn truncated_tool_json_emits_error_not_guess() {
        let mut a = AnthropicStreamAssembler::new();
        a.on_event(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"t1","name":"run_command","input":{}}}"#,
        );
        a.on_event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"rm"}}"#,
        );
        let evs = a.on_event(
            "message_delta",
            r#"{"delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":5}}"#,
        );
        assert!(evs.iter().any(|e| matches!(
            e,
            ModelEvent::Error {
                error
            } if error.kind == ModelErrorKind::Truncated
        )));
        assert!(
            !evs.iter()
                .any(|e| matches!(e, ModelEvent::ToolCallCompleted { .. }))
        );
    }

    #[test]
    fn finalize_is_idempotent() {
        let mut a = AnthropicStreamAssembler::new();
        let first = a.on_event("message_delta", r#"{"delta":{"stop_reason":"end_turn"}}"#);
        assert!(
            first
                .iter()
                .any(|e| matches!(e, ModelEvent::MessageCompleted { .. }))
        );
        let second = a.on_event("message_stop", "{}");
        assert!(
            !second
                .iter()
                .any(|e| matches!(e, ModelEvent::MessageCompleted { .. }))
        );
    }
}
