//! `leveler-remote-agent` — the host-side bridge for remote APP control.
//!
//! Runs on the developer's machine beside the runtime. It holds the runtime's
//! signing key and the record of which devices the user accepted, and it is the
//! only component that decides whether a frame arriving from a relay may become
//! a command.
//!
//! It deliberately does **not** depend on `leveler-web`: reaching the session
//! frame types through the browser server would drag axum and the embedded SPA
//! into a process whose entire job is to be small and auditable. The framing
//! comes from `leveler-session-wire` instead.
#![forbid(unsafe_code)]

mod approvals;
mod attachments;
mod audit;
mod bridge;
mod config;
mod devices;
mod projects;
mod tunnel;

pub use approvals::WAITER_POLL;
pub use attachments::{
    FETCH_CHUNK_BYTES, FetchChunkRequest, FetchChunkResponse, MAX_ATTACHMENT_BYTES,
    MAX_SESSION_BYTES, UploadChunk, UploadError, is_sha256_hex,
};
pub use audit::{AuditEvent, AuditLog, DEFAULT_RETENTION_DAYS, hashed};
pub use bridge::{AdmissionError, Admitted, AgentBridge};
pub use config::{
    ConfigError, DEFAULT_APPROVAL_TIMEOUT_SECS, RemoteConfig, RemoteHome, runtime_id_for,
};
pub use devices::{TrustError, TrustedDevices};
#[cfg(unix)]
pub use projects::ProjectRouter;
pub use projects::{ProjectInfo, ProjectRoutes, RouteError, SingleProject};
pub use tunnel::{TunnelError, run_tunnel};
