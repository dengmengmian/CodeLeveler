//! `leveler-test-support` — a scriptable mock provider server.
//!
//! Lets integration tests drive the real HTTP transport + protocol decoder
//! against deterministic responses: clean SSE streams, mid-stream interruption,
//! HTTP 429/5xx, fragmented writes, and malformed JSON (spec §48, §53.15-16).
#![forbid(unsafe_code)]

mod mock_server;

pub use mock_server::{MockResponse, MockServer};
