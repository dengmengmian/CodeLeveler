//! `leveler-test-support` — shared test fixtures.
//!
//! - [`MockServer`]: a scriptable mock OpenAI-compatible provider server.
//!   Lets integration tests drive the real HTTP transport + protocol decoder
//!   against deterministic responses: clean SSE streams, mid-stream
//!   interruption, HTTP 429/5xx, fragmented writes, and malformed JSON
//!   (spec §48, §53.15-16).
//! - [`TestBoots`]: boot liveness for tests that play several boots of one
//!   runtime in a single process.
//! - [`git`]: throwaway git repositories isolated from the host's git config.
//! - [`shell_fixture`]: the same trivial process — print a line, stay alive —
//!   spelled for whichever host is running the test.
//! - [`sandbox`]: whether a test is already inside a verification sandbox, so
//!   a test that needs to observe confinement can stand down instead of
//!   reporting the platform's nesting limit as a defect.
#![forbid(unsafe_code)]

mod boots;
pub mod git;
mod mock_server;
pub mod sandbox;
pub mod shell_fixture;

pub use boots::TestBoots;
pub use mock_server::{MockResponse, MockServer};
pub use sandbox::already_confined;
pub use shell_fixture::{dual_stream_command, echo_command, sleep_command, sleep_shell_line};
