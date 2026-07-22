//! `leveler-protocol` — vendor wire-protocol adapters.
//!
//! Includes [`OpenAiChatAdapter`], implementing the OpenAI
//! Chat Completions protocol (which DeepSeek and GLM both expose). The crate is
//! independent of any provider transport or agent (spec §13): it converts
//! between unified [`leveler_model`] types and the wire format, and provides a
//! byte-fragmentation-tolerant [`sse::SseDecoder`].
#![forbid(unsafe_code)]

pub mod anthropic_messages;
pub mod openai_chat;
pub mod sse;

pub use anthropic_messages::AnthropicMessagesAdapter;
pub use openai_chat::OpenAiChatAdapter;
pub use sse::{SseDecodeError, SseDecoder, SseEvent};
