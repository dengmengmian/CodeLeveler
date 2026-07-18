//! Unified message and tool-call content model (spec §10.3, §10.4).

use serde::{Deserialize, Serialize};

use leveler_core::ToolCallId;

/// The role of a message in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message, composed of one or more content parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl Message {
    /// Convenience constructor for a plain text message.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::Text { text: text.into() }],
        }
    }

    /// Concatenate all `Text` parts (ignoring reasoning/tool parts).
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// A discrete piece of message content. The enum is intentionally wider than
/// the currently supported blocks (`Text`/`ToolCall`/`ToolResult`) so protocol types
/// never need to change to add images or reasoning later (spec §10.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    Reasoning { text: String },
    Image { source: ImageSource },
    ToolCall { call: ToolCall },
    ToolResult { result: ToolResultContent },
}

/// Where an image comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    Url { url: String },
    Base64 { media_type: String, data: String },
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A concrete tool invocation produced by the model.
///
/// The `id` must be stable across streaming reassembly; `arguments` is only
/// ever populated once the streamed JSON fragments have been fully joined and
/// validated (spec §10.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// The result of executing a tool, fed back to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultContent {
    pub call_id: ToolCallId,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

/// How the model should decide whether to call tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides freely.
    #[default]
    Auto,
    /// Model must not call any tool.
    None,
    /// Model must call at least one tool.
    Required,
    /// Model must call this specific tool.
    Tool(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_part_tagged_serialization() {
        let part = ContentPart::Text { text: "hi".into() };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hi");
    }

    #[test]
    fn text_content_joins_only_text_parts() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::Reasoning {
                    text: "think".into(),
                },
                ContentPart::Text { text: "a".into() },
                ContentPart::Text { text: "b".into() },
            ],
        };
        assert_eq!(msg.text_content(), "ab");
    }

    #[test]
    fn tool_choice_defaults_to_auto() {
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
    }
}
