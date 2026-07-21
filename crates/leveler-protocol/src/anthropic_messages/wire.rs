//! Wire types for the Anthropic Messages protocol (`/v1/messages`). These are
//! *only* used inside this adapter; nothing outside the protocol crate sees them.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    /// REQUIRED by Anthropic (unlike OpenAI, where it is optional).
    pub max_tokens: u32,
    /// System prompt is a top-level field, not a message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<ReqMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ReqTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReqMessage {
    /// "user" or "assistant" — Anthropic has no "system"/"tool" roles.
    pub role: String,
    pub content: Vec<ReqBlock>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReqBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    Image {
        source: ImageBlockSource,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageBlockSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ReqTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Non-streaming response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct MessagesResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub content: Vec<RespBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RespBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    /// Forward-compatible catch-all (e.g. `redacted_thinking`).
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Prompt-cache hits (Anthropic's spelling).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

// ---------------------------------------------------------------------------
// Streaming events (dispatched by the SSE `event:` field)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct StreamMessageStart {
    pub message: StreamMessageMeta,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamMessageMeta {
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamContentBlockStart {
    pub index: usize,
    pub content_block: RespBlock,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamContentBlockDelta {
    pub index: usize,
    pub delta: BlockDelta,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockDelta {
    TextDelta {
        #[serde(default)]
        text: String,
    },
    InputJsonDelta {
        #[serde(default)]
        partial_json: String,
    },
    ThinkingDelta {
        #[serde(default)]
        thinking: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamMessageDelta {
    #[serde(default)]
    pub delta: MessageDeltaBody,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MessageDeltaBody {
    #[serde(default)]
    pub stop_reason: Option<String>,
}
