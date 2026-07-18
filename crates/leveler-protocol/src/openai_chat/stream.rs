//! Streaming decode: turns OpenAI chat SSE chunks into unified [`ModelEvent`]s,
//! reassembling tool-call arguments that arrive fragmented across many chunks
//! (spec §10.4, §53.11-12).

use std::collections::BTreeMap;

use leveler_core::ToolCallId;
use leveler_model::{FinishReason, ModelError, ModelErrorKind, ModelEvent, TokenUsage, ToolCall};

use super::wire::ChatChunk;

/// Map an OpenAI finish-reason string to the unified enum.
pub(super) fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

/// Per-tool-call assembly state, keyed by streaming `index`.
#[derive(Debug, Default)]
struct ToolCallState {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    started_emitted: bool,
}

/// Stateful assembler consuming decoded chunks and emitting unified events.
#[derive(Debug, Default)]
pub struct ChatStreamAssembler {
    tool_calls: BTreeMap<usize, ToolCallState>,
    completed: bool,
}

impl ChatStreamAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one decoded chunk, returning the events it produced (in order).
    pub fn on_chunk(&mut self, chunk: ChatChunk) -> Vec<ModelEvent> {
        let mut events = Vec::new();
        let mut finish_reason: Option<String> = None;

        for choice in chunk.choices {
            let delta = choice.delta;

            if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
                events.push(ModelEvent::TextDelta { delta: text });
            }
            if let Some(reasoning) = delta.reasoning_content.filter(|t| !t.is_empty()) {
                events.push(ModelEvent::ReasoningDelta { delta: reasoning });
            }

            for tc in delta.tool_calls {
                let index = tc.index;
                let state = self.tool_calls.entry(index).or_default();
                if let Some(id) = tc.id.filter(|s| !s.is_empty()) {
                    state.id = Some(id);
                }
                if let Some(func) = tc.function {
                    if let Some(name) = func.name.filter(|s| !s.is_empty()) {
                        state.name = Some(name);
                    }
                    if let Some(args) = func.arguments.filter(|s| !s.is_empty()) {
                        if !state.started_emitted {
                            events.push(ModelEvent::ToolCallStarted {
                                index,
                                id: state.id.clone().map(ToolCallId::new),
                                name: state.name.clone(),
                            });
                            state.started_emitted = true;
                        }
                        events.push(ModelEvent::ToolCallArgumentsDelta {
                            index,
                            delta: args.clone(),
                        });
                        state.arguments.push_str(&args);
                        continue;
                    }
                }
                // A tool-call slot opened with only id/name and no arg fragment yet.
                if !state.started_emitted && (state.id.is_some() || state.name.is_some()) {
                    events.push(ModelEvent::ToolCallStarted {
                        index,
                        id: state.id.clone().map(ToolCallId::new),
                        name: state.name.clone(),
                    });
                    state.started_emitted = true;
                }
            }

            // Defer finalization: DeepSeek puts `usage` in this same chunk, and a
            // consumer that stops at `MessageCompleted` would never see it.
            if let Some(reason) = choice.finish_reason {
                finish_reason = Some(reason);
            }
        }

        if let Some(usage) = chunk.usage {
            events.push(ModelEvent::UsageUpdated {
                usage: TokenUsage {
                    input_tokens: usage.prompt_tokens,
                    output_tokens: usage.completion_tokens,
                    cached_input_tokens: usage.cached_input_tokens(),
                },
            });
        }

        if let Some(reason) = finish_reason {
            events.extend(self.finalize(&reason));
        }

        events
    }

    /// Emit completed tool calls (parsing their joined arguments) followed by
    /// the terminal `MessageCompleted`. Malformed argument JSON becomes an
    /// `Error` event — never a guessed execution (spec §10.4).
    fn finalize(&mut self, reason: &str) -> Vec<ModelEvent> {
        if self.completed {
            return Vec::new();
        }
        self.completed = true;
        let mut events = Vec::new();

        for (index, state) in std::mem::take(&mut self.tool_calls) {
            let Some(name) = state.name else {
                // No function name ever arrived — nothing callable to complete.
                continue;
            };
            // Empty arguments are represented as an empty JSON object.
            let raw = if state.arguments.trim().is_empty() {
                "{}"
            } else {
                &state.arguments
            };
            // Strict JSON only. Never repair-and-execute: a guessed shell /
            // patch / delete payload would violate the execution boundary.
            // Decode errors are fed back by the agent so the model can resend.
            match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(arguments) => {
                    let id = state
                        .id
                        .map(ToolCallId::new)
                        .unwrap_or_else(|| ToolCallId::new(format!("call_{index}")));
                    events.push(ModelEvent::ToolCallCompleted {
                        call: ToolCall {
                            id,
                            name,
                            arguments,
                        },
                    });
                }
                Err(e) => {
                    // A `length` finish means the model hit its output-token cap
                    // mid-arguments, so the JSON is truncated (not malformed).
                    // Say so plainly instead of surfacing a raw parse error.
                    let error = if reason == "length" {
                        ModelError::new(
                            ModelErrorKind::Truncated,
                            format!(
                                "输出被 max_output_tokens 上限截断,工具调用 `{name}` 的参数不完整（提高该模型的 max_output_tokens,或让改动分批更小）"
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
            finish_reason: map_finish_reason(reason),
        });
        events
    }

    /// Whether a terminal `MessageCompleted` has already been emitted.
    pub fn is_completed(&self) -> bool {
        self.completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(json: serde_json::Value) -> ChatChunk {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn text_deltas_are_emitted() {
        let mut a = ChatStreamAssembler::new();
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"content": "Hel"}}]
        })));
        assert_eq!(
            evs,
            vec![ModelEvent::TextDelta {
                delta: "Hel".into()
            }]
        );
    }

    #[test]
    fn tool_call_arguments_reassembled_across_chunks() {
        let mut a = ChatStreamAssembler::new();
        a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "grep", "arguments": "{\"pat"}}
            ]}}]
        })));
        a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "tern\":\"x\"}"}}
            ]}}]
        })));
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        })));
        let completed = evs
            .iter()
            .find_map(|e| match e {
                ModelEvent::ToolCallCompleted { call } => Some(call),
                _ => None,
            })
            .expect("tool call should complete");
        assert_eq!(completed.name, "grep");
        assert_eq!(completed.id.as_str(), "call_1");
        assert_eq!(completed.arguments["pattern"], "x");
    }

    #[test]
    fn invalid_tool_arguments_produce_error_not_guess() {
        let mut a = ChatStreamAssembler::new();
        a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "c1", "function": {"name": "f", "arguments": "{not json"}}
            ]}}]
        })));
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        })));
        assert!(evs.iter().any(|e| matches!(e, ModelEvent::Error { .. })));
        assert!(
            !evs.iter()
                .any(|e| matches!(e, ModelEvent::ToolCallCompleted { .. }))
        );
    }

    #[test]
    fn missing_tool_id_gets_stable_fallback() {
        let mut a = ChatStreamAssembler::new();
        a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"name": "f", "arguments": "{}"}}
            ]}}]
        })));
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        })));
        let call = evs
            .iter()
            .find_map(|e| match e {
                ModelEvent::ToolCallCompleted { call } => Some(call),
                _ => None,
            })
            .unwrap();
        assert_eq!(call.id.as_str(), "call_0");
    }

    #[test]
    fn usage_is_surfaced() {
        let mut a = ChatStreamAssembler::new();
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3}
        })));
        assert_eq!(
            evs,
            vec![ModelEvent::UsageUpdated {
                usage: TokenUsage {
                    input_tokens: 12,
                    output_tokens: 3,
                    cached_input_tokens: 0,
                }
            }]
        );
    }

    /// DeepSeek sends `usage` in the same chunk that carries `finish_reason`.
    /// The consumer stops reading at `MessageCompleted`, so usage must be emitted
    /// before it or the whole turn reports no tokens at all.
    /// Invalid escapes must not be "repaired" into a ToolCallCompleted — that
    /// would execute guessed shell/patch arguments. Emit Decode so the agent
    /// can feed the error back and let the model resend valid JSON.
    #[test]
    fn stray_shell_escape_in_tool_args_emits_decode_error() {
        let mut a = ChatStreamAssembler::new();
        a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1",
                "function": {
                    "name": "run_command",
                    "arguments": r#"{"command":"curl -w \%{http_code} https://docs.atomgit.com/openapi"}"#
                }
            }]}}]
        })));
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        })));

        assert!(
            evs.iter().any(|e| matches!(e, ModelEvent::Error { .. })),
            "must emit Error, got {evs:?}"
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(e, ModelEvent::ToolCallCompleted { .. })),
            "must not complete a tool call from repaired JSON, got {evs:?}"
        );
    }

    /// Bare control characters inside a JSON string are invalid. Never guess
    /// escapes and execute — surface Decode for agent-level retry.
    #[test]
    fn apply_patch_body_with_bare_newlines_emits_decode_error() {
        let mut a = ChatStreamAssembler::new();
        // A raw newline inside the JSON string — invalid JSON, not a tool call.
        let args =
            "{\"patch\":\"*** Begin Patch\n*** Update File: src/lib.rs\n-a\n+b\n*** End Patch\"}";
        a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1",
                "function": {"name": "apply_patch", "arguments": args}
            }]}}]
        })));
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        })));

        assert!(
            evs.iter().any(|e| matches!(e, ModelEvent::Error { .. })),
            "must emit Error, got {evs:?}"
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(e, ModelEvent::ToolCallCompleted { .. })),
            "must not complete a tool call from repaired JSON, got {evs:?}"
        );
    }

    /// Markdown-fenced / prose-wrapped args are also not valid JSON arguments.
    #[test]
    fn markdown_fenced_tool_args_emit_decode_error() {
        let mut a = ChatStreamAssembler::new();
        a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1",
                "function": {
                    "name": "grep",
                    "arguments": "```json\n{\"pattern\": \"foo\"}\n```"
                }
            }]}}]
        })));
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        })));
        assert!(evs.iter().any(|e| matches!(e, ModelEvent::Error { .. })));
        assert!(
            !evs.iter()
                .any(|e| matches!(e, ModelEvent::ToolCallCompleted { .. }))
        );
    }

    #[test]
    fn usage_precedes_message_completed_in_the_same_chunk() {
        let mut a = ChatStreamAssembler::new();
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 85,
                "completion_tokens": 32,
                "prompt_cache_hit_tokens": 64
            }
        })));

        let usage_at = evs
            .iter()
            .position(|e| matches!(e, ModelEvent::UsageUpdated { .. }))
            .expect("usage event must be emitted");
        let completed_at = evs
            .iter()
            .position(|e| matches!(e, ModelEvent::MessageCompleted { .. }))
            .expect("completion event must be emitted");
        assert!(
            usage_at < completed_at,
            "usage must precede completion, got {evs:?}"
        );
    }

    #[test]
    fn deepseek_prompt_cache_hit_is_surfaced() {
        let mut a = ChatStreamAssembler::new();
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 3,
                "prompt_cache_hit_tokens": 90,
                "prompt_cache_miss_tokens": 10
            }
        })));
        let ModelEvent::UsageUpdated { usage } = &evs[0] else {
            panic!("expected usage event, got {evs:?}");
        };
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cached_input_tokens, 90);
    }

    /// OpenAI reports the same thing under `prompt_tokens_details.cached_tokens`.
    #[test]
    fn openai_cached_tokens_detail_is_surfaced() {
        let mut a = ChatStreamAssembler::new();
        let evs = a.on_chunk(chunk(serde_json::json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 3,
                "prompt_tokens_details": {"cached_tokens": 64}
            }
        })));
        let ModelEvent::UsageUpdated { usage } = &evs[0] else {
            panic!("expected usage event, got {evs:?}");
        };
        assert_eq!(usage.cached_input_tokens, 64);
    }

    #[test]
    fn finalize_is_idempotent() {
        let mut a = ChatStreamAssembler::new();
        let first = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]
        })));
        assert!(
            first
                .iter()
                .any(|e| matches!(e, ModelEvent::MessageCompleted { .. }))
        );
        let second = a.on_chunk(chunk(serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}]
        })));
        assert!(
            !second
                .iter()
                .any(|e| matches!(e, ModelEvent::MessageCompleted { .. }))
        );
    }
}
