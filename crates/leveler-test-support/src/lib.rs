//! `leveler-test-support` — shared test fixtures.
//!
//! - [`MockServer`]: a scriptable mock OpenAI-compatible provider server.
//!   Lets integration tests drive the real HTTP transport + protocol decoder
//!   against deterministic responses: clean SSE streams, mid-stream
//!   interruption, HTTP 429/5xx, fragmented writes, and malformed JSON
//!   (spec §48, §53.15-16).
//! - [`git`]: throwaway git repositories isolated from the host's git config.
#![forbid(unsafe_code)]

pub mod git;
mod mock_server;

pub use mock_server::{MockResponse, MockServer};
