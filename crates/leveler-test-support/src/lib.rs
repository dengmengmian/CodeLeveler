//! `leveler-test-support` — shared test fixtures.
//!
//! - [`MockServer`]: a scriptable mock OpenAI-compatible provider server.
//!   Lets integration tests drive the real HTTP transport + protocol decoder
//!   against deterministic responses: clean SSE streams, mid-stream
//!   interruption, HTTP 429/5xx, fragmented writes, and malformed JSON
//!   (spec §48, §53.15-16).
//! - [`git`]: throwaway git repositories isolated from the host's git config.
//! - [`shell_fixture`]: the same trivial process — print a line, stay alive —
//!   spelled for whichever host is running the test.
#![forbid(unsafe_code)]

pub mod git;
mod mock_server;
pub mod shell_fixture;

pub use mock_server::{MockResponse, MockServer};
pub use shell_fixture::{dual_stream_command, echo_command, sleep_command, sleep_shell_line};
