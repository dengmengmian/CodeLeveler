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
mod bridge;
mod devices;
mod projects;
mod tunnel;

pub use approvals::WAITER_POLL;
pub use bridge::{AdmissionError, Admitted, AgentBridge};
pub use devices::{TrustError, TrustedDevices};
#[cfg(unix)]
pub use projects::ProjectRouter;
pub use projects::{ProjectInfo, ProjectRoutes, RouteError, SingleProject};
pub use tunnel::{TunnelError, run_tunnel};
