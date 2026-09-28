//! Unified message and tool-call content model (spec §10.3, §10.4).

use serde::{Deserialize, Serialize};

use leveler_core::ToolCallId;

use crate::authority::TranscriptOrigin;

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
///
/// `origin` is host metadata. It rides in the persisted payload so a restored
/// transcript can tell user input from a harness row that used the user
/// transport role. Protocol encoders do not read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
    /// Absent on historical payloads. Absence is not user input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<TranscriptOrigin>,
}

impl Message {
    /// Convenience constructor for a plain text message.
    ///
    /// This records no origin. A user-role row built here is historical-shaped:
    /// it does not become user intent on its own. Production user input uses
    /// [`Self::user_input`].
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::from_parts(role, vec![ContentPart::Text { text: text.into() }], None)
    }

    /// Text a person submitted.
    pub fn user_input(text: impl Into<String>) -> Self {
        Self::from_parts(
            Role::User,
            vec![ContentPart::Text { text: text.into() }],
            Some(TranscriptOrigin::UserInput),
        )
    }

    /// A user-transport row whose source is already known.
    pub fn user(text: impl Into<String>, origin: TranscriptOrigin) -> Self {
        Self::from_parts(
            Role::User,
            vec![ContentPart::Text { text: text.into() }],
            Some(origin),
        )
    }

    /// Build a message without inferring authority from `role`.
    pub fn from_parts(
        role: Role,
        content: Vec<ContentPart>,
        origin: Option<TranscriptOrigin>,
    ) -> Self {
        Self {
            role,
            content,
            origin,
        }
    }

    /// Preserve observed output without claiming that generation completed.
    /// The host adds this notice; unfinished tool or reasoning fragments are
    /// never promoted into a replayable assistant message.
    pub fn interrupted_response(text: &str) -> Self {
        Self::text(
            Role::Assistant,
            format!(
                "{text}\n\n[Response interrupted before completion. This is incomplete output; no tool calls from this response were executed.]"
            ),
        )
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
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    /// Provider-authenticated thinking. Signature is opaque and replayed verbatim.
    SignedReasoning {
        text: String,
        signature: String,
    },
    /// Opaque thinking payload that must survive tool/multi-turn round trips.
    RedactedReasoning {
        data: String,
    },
    Image {
        source: ImageSource,
    },
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        result: ToolResultContent,
    },
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

impl ToolChoice {
    /// Whether this choice *forces* a tool call (`Required` or a named tool),
    /// as opposed to leaving the decision to the model (`Auto`) or forbidding
    /// calls (`None`). Some providers reject forced choices in thinking mode —
    /// see `CompatibilityConfig::thinking_supports_forced_tool_choice`.
    pub fn forces_tool_call(&self) -> bool {
        matches!(self, Self::Required | Self::Tool(_))
    }
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
            origin: None,
        };
        assert_eq!(msg.text_content(), "ab");
    }

    #[test]
    fn tool_choice_defaults_to_auto() {
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
    }

    #[test]
    fn legacy_user_payload_omits_origin_and_new_input_keeps_it() {
        let legacy = r#"{"role":"user","content":[{"type":"text","text":"hello"}]}"#;
        let restored: Message = serde_json::from_str(legacy).unwrap();
        assert_eq!(restored.role, Role::User);
        assert!(restored.origin.is_none());
        let bare = serde_json::to_string(&Message::text(Role::User, "hello")).unwrap();
        assert!(!bare.contains("origin"));
        let payload = serde_json::to_string(&Message::user_input("hello")).unwrap();
        let round: Message = serde_json::from_str(&payload).unwrap();
        assert_eq!(round.origin, Some(crate::TranscriptOrigin::UserInput));
        assert_eq!(round.text_content(), "hello");
    }
}
