//! Convert persisted `leveler-model` message rows into the neutral transcript
//! items the memory extractor consumes.
//!
//! The memory crate owns the condensation algorithm and its byte budget; this
//! module owns the storage-schema mapping. Rows the conversation never showed
//! the model as a person's words — protocol repairs, runtime notices, fold
//! breadcrumbs — are excluded so they can never be mistaken for user intent.
//! An unparseable payload fails the job instead of silently shrinking the
//! evidence.

use std::collections::HashMap;

use leveler_memory::capture_transcript::{TranscriptItem, TranscriptToolCall};
use leveler_model::{ContentPart, ImageSource, Message, Role, TranscriptOrigin};

/// Convert one turn range's stored payloads, in durable order.
///
/// The returned items drop the standing system prompt (it is not conversation)
/// and any harness-authored user-transport row. Reasoning is kept here because
/// the memory crate discards it first under budget pressure.
pub(crate) fn transcript_items(payloads: &[String]) -> Result<Vec<TranscriptItem>, String> {
    let mut messages = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let message: Message = serde_json::from_str(payload)
            .map_err(|error| format!("stored message payload is not a message: {error}"))?;
        messages.push(message);
    }

    // Tool results carry only a call id in storage; recover the tool name from
    // the assistant call that produced them.
    let mut tool_names = HashMap::new();
    for message in &messages {
        for part in &message.content {
            if let ContentPart::ToolCall { call } = part {
                tool_names.insert(call.id.as_str().to_owned(), call.name.clone());
            }
        }
    }

    let mut items = Vec::new();
    for message in messages {
        if message.role == Role::System {
            continue;
        }
        if matches!(
            message.origin,
            Some(
                TranscriptOrigin::ProtocolRepair { .. }
                    | TranscriptOrigin::RuntimeNotice { .. }
                    | TranscriptOrigin::BackgroundTaskResults { .. }
                    | TranscriptOrigin::CompactionSummary { .. }
            )
        ) {
            continue;
        }
        match message.role {
            Role::User => {
                let text = message.text_content();
                let text = text.trim();
                if !text.is_empty() {
                    items.push(TranscriptItem::User {
                        text: text.to_owned(),
                    });
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for part in message.content {
                    match part {
                        ContentPart::Text { text: part } => text.push_str(&part),
                        ContentPart::ToolCall { call } => tool_calls.push(TranscriptToolCall {
                            name: call.name,
                            arguments: call.arguments.to_string(),
                        }),
                        ContentPart::Reasoning { text } => {
                            items.push(TranscriptItem::Reasoning { text });
                        }
                        ContentPart::SignedReasoning { text, .. } => {
                            items.push(TranscriptItem::Reasoning { text });
                        }
                        ContentPart::RedactedReasoning { .. } => {}
                        ContentPart::Image { source } => {
                            items.push(image_item(source));
                        }
                        ContentPart::ToolResult { .. } => {}
                    }
                }
                let text = text.trim();
                if !text.is_empty() || !tool_calls.is_empty() {
                    items.push(TranscriptItem::Assistant {
                        text: text.to_owned(),
                        tool_calls,
                    });
                }
            }
            Role::Tool => {
                for part in message.content {
                    let ContentPart::ToolResult { result } = part else {
                        continue;
                    };
                    items.push(TranscriptItem::ToolResult {
                        tool: tool_names
                            .get(result.call_id.as_str())
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_owned()),
                        content: result.content,
                        is_error: result.is_error,
                    });
                }
            }
            Role::System => {}
        }
    }
    Ok(items)
}

fn image_item(source: ImageSource) -> TranscriptItem {
    match source {
        ImageSource::Url { .. } => TranscriptItem::Image {
            media_type: "image/url".to_owned(),
            byte_len: 0,
        },
        ImageSource::Base64 { media_type, data } => TranscriptItem::Image {
            media_type,
            byte_len: data.len(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::ToolCallId;
    use leveler_model::{ToolCall, ToolResultContent};

    fn payload(message: &Message) -> String {
        serde_json::to_string(message).expect("message serializes")
    }

    #[test]
    fn tool_results_recover_the_calling_tool_name() {
        let call = ToolCall {
            id: ToolCallId::new("call_1"),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };
        let assistant = Message::from_parts(
            Role::Assistant,
            vec![
                ContentPart::Text {
                    text: "checking".to_owned(),
                },
                ContentPart::ToolCall { call },
            ],
            None,
        );
        let tool = Message::from_parts(
            Role::Tool,
            vec![ContentPart::ToolResult {
                result: ToolResultContent {
                    call_id: ToolCallId::new("call_1"),
                    content: "fn main() {}".to_owned(),
                    is_error: false,
                },
            }],
            None,
        );
        let items = transcript_items(&[payload(&assistant), payload(&tool)]).expect("items");
        assert_eq!(
            items,
            vec![
                TranscriptItem::Assistant {
                    text: "checking".to_owned(),
                    tool_calls: vec![TranscriptToolCall {
                        name: "read_file".to_owned(),
                        arguments: "{\"path\":\"src/lib.rs\"}".to_owned(),
                    }],
                },
                TranscriptItem::ToolResult {
                    tool: "read_file".to_owned(),
                    content: "fn main() {}".to_owned(),
                    is_error: false,
                },
            ]
        );
    }

    #[test]
    fn harness_notices_are_not_user_intent() {
        let repair = Message::from_parts(
            Role::User,
            vec![ContentPart::Text {
                text: "protocol repair".to_owned(),
            }],
            Some(TranscriptOrigin::RuntimeNotice {
                notice: leveler_model::RuntimeNoticeKind::CrashRecovery,
            }),
        );
        assert!(
            transcript_items(&[payload(&repair)])
                .expect("items")
                .is_empty()
        );
    }

    #[test]
    fn a_corrupt_payload_fails_instead_of_shrinking_the_evidence() {
        let error = transcript_items(&["not json".to_owned()]).unwrap_err();
        assert!(error.contains("stored message payload"));
    }
}
