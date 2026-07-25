//! `leveler-relay` — the self-hosted edge for CodeLeveler remote control.
//!
//! Its whole job is reachability. A developer machine sits behind NAT, so the
//! agent dials out and the relay introduces a paired phone to that outbound
//! connection. What the relay must **not** be is a control plane: the design
//! treats it as potentially compromised, and every security-relevant payload
//! that passes through it is inside a device- or runtime-signed envelope it
//! cannot forge.
//!
//! Consequences that shape this crate:
//!
//! - **No command queue, no transcript.** A queue would let a command outlive
//!   the revocation of the device that sent it; a transcript would put session
//!   content on this machine. An offline host produces 503 and the device
//!   retries.
//! - **Tokens authorize routing only.** A token gets a caller to a stream; it
//!   never makes a frame legitimate. The agent still verifies every signature.
//! - **Payloads stay opaque.** The relay forwards signed envelopes verbatim; it
//!   does not unwrap one to re-wrap it in JSON of its own, which would strip the
//!   very property that makes the frame trustworthy.
#![forbid(unsafe_code)]

mod routes;
mod state;

pub use routes::build_router;
pub use state::{
    CONFIRM_TTL_SECS, ClaimedBy, DeviceRecord, PAIRING_TTL_SECS, Pairing, RelayError, RelayState,
    RuntimeOnline,
};
