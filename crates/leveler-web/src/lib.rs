//! `leveler-web` — the browser WebUI server for CodeLeveler.
//!
//! Serves the single-page WebUI and bridges it to the runtime over the stable
//! client protocol: a small set of token-authenticated REST endpoints plus one
//! WebSocket carrying [`leveler_client_protocol::ClientCommand`]s upstream and
//! [`leveler_client_protocol::RuntimeEvent`]s / snapshots downstream. The
//! server only ever talks to a [`leveler_local_transport::LocalRuntimeService`],
//! so the same code backs an in-process runtime and a `leveler serve --tcp`
//! daemon.
//!
//! Security posture: the listener is loopback-only, every endpoint (REST and
//! the WS upgrade) requires a 256-bit bearer token compared in constant time,
//! and the token is minted per process run and printed once — never persisted.
#![forbid(unsafe_code)]

mod auth;
mod projects;
mod protocol;
mod repo;
mod router;
mod server;
mod ws;

use std::net::SocketAddr;

pub use projects::{ProjectError, ProjectInfo, ProjectManager, ProjectStatus};
pub use protocol::{DownstreamMessage, UpstreamMessage};
pub use router::RouterService;
pub use server::{WebServer, bind, bind_multi, build_router, build_router_multi, serve};

/// Errors the WebUI server can fail with.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// The server was asked to start without a bearer token.
    #[error("refusing to start the WebUI server with an empty token")]
    EmptyToken,
    /// The requested bind address is not a loopback address.
    #[error("refusing to bind the WebUI server to a non-loopback address: {0}")]
    NonLoopback(SocketAddr),
    /// Binding or serving failed at the io layer.
    #[error("web server io error: {0}")]
    Io(#[from] std::io::Error),
}
