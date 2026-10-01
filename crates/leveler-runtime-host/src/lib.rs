//! Shared local runtime host lifecycle. Shells provide presentation and
//! process ownership policy; this crate owns discovery, handoff, boot and
//! readiness mechanics.
#![forbid(unsafe_code)]

mod client;
mod server;
#[cfg(any(unix, windows))]
pub use client::{
    DaemonReviver, DetachedRuntimeLaunch, connect_global_task_runtime, ensure_default_runtime,
    observe_retiring_runtime,
};
pub use client::{
    EnsureError, HandoffAction, HandoffEvent, HandoffUi, RuntimeConsistency, classify_runtime,
    classify_runtime_generation, force_handover_allowed, handoff_key, probe_default_runtime,
    stalled_turn_sessions, verify_replacement,
};
mod owned;
pub use owned::{
    EnsureOwnedError, OwnedRuntime, OwnedRuntimeError, OwnedRuntimeLaunch,
    connect_existing_runtime, ensure_owned_runtime,
};
pub use server::{
    BoundServers, DAEMON_TOKEN_ENV, PreparedDaemon, bind_daemon_transports, daemon_idle_timeout,
    generate_daemon_token, prepare_daemon, ready_json,
};

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn daemon_token_is_os_random_256_bit_hex() {
        let first = generate_daemon_token();
        let second = generate_daemon_token();
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn ready_contract_keeps_existing_fields() {
        let value = ready_json(std::path::Path::new("/tmp/runtime.sock"), None, "rid").unwrap();
        assert!(value.get("pid").and_then(|value| value.as_u64()).is_some());
        assert_eq!(value["socket"], "/tmp/runtime.sock");
        assert!(value["addr"].is_null());
        assert!(value["token"].is_null());
        assert_eq!(value["runtime_id"], "rid");
    }
}
