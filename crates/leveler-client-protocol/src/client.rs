//! The transport trait: how a client talks to the runtime.

use async_trait::async_trait;
use tokio::sync::broadcast;

use leveler_core::{CommandId, SessionId};

use super::command::ClientCommand;
use super::command_envelope::CommandEnvelope;
use super::event::RuntimeEvent;
use super::snapshot::UiSessionSnapshot;
use super::version::ProtocolEnvelope;

/// Errors a client operation can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The requested session does not exist.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),
    /// The runtime rejected or failed to accept the command.
    #[error("runtime error: {0}")]
    Runtime(String),
}

/// The seam between UI clients and the CodeLeveler runtime.
///
/// Implementations bridge the runtime's internal mechanics (synchronous
/// observer callbacks, cancellation tokens, approvers) into this async,
/// broadcast-shaped contract. Implementations may run in-process or bridge a
/// local daemon through a socket without changing clients.
#[async_trait]
pub trait InteractiveRuntimeClient: Send + Sync {
    /// Send a command into the runtime for immediate dispatch. This is the raw
    /// path used by tests and by [`Self::deliver`] after idempotency checks.
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError>;

    /// Deliver a command wrapped in its idempotency/versioning [`CommandEnvelope`]
    /// — the path production clients use. The default implementation just
    /// dispatches (for mocks); a real runtime overrides this to dedup by
    /// `command_id` (at-least-once delivery) and check `expected_version`
    /// (optimistic concurrency) before dispatching.
    async fn deliver(&self, envelope: CommandEnvelope) -> Result<(), ClientError> {
        self.send(envelope.command).await
    }

    /// Validate the top-level wire version before handing a command envelope to
    /// the runtime. Local clients use this same boundary, so version validation
    /// is exercised before a remote transport exists; a socket/cloud adapter can
    /// deserialize and call this method without inventing another code path.
    async fn deliver_protocol(
        &self,
        envelope: ProtocolEnvelope<CommandEnvelope>,
    ) -> Result<(), ClientError> {
        let command = envelope
            .into_body()
            .map_err(|error| ClientError::Runtime(error.to_string()))?;
        self.deliver(command).await
    }

    /// Convenience for local (in-process) issuers: wrap `command` in an envelope
    /// targeting `session_id`, with a fresh id and no version check (a single
    /// local client has no concurrent writer to race), then [`Self::deliver`] it.
    /// This is the path the TUI/CLI use so every command flows through the
    /// idempotency envelope, not a raw `send`.
    async fn issue(
        &self,
        session_id: SessionId,
        command: ClientCommand,
    ) -> Result<(), ClientError> {
        self.deliver_protocol(ProtocolEnvelope::wrap(CommandEnvelope {
            command_id: CommandId::generate(),
            session_id,
            expected_version: None,
            issued_at: leveler_core::now().to_rfc3339(),
            command,
        }))
        .await
    }

    /// Subscribe to the runtime event stream. Each subscriber gets its own
    /// receiver; late subscribers should follow up with [`Self::snapshot`].
    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent>;

    /// Subscribe only to events owned by one session. Legacy clients default
    /// to the all-events stream; daemon-capable runtimes override this so two
    /// concurrent sessions cannot consume each other's model/tool output.
    fn subscribe_session(&self, _session_id: &SessionId) -> broadcast::Receiver<RuntimeEvent> {
        self.subscribe()
    }

    /// Fetch the current snapshot for a session (for initial render / resync
    /// after a lagged subscription).
    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_not_found_display_includes_id() {
        let err = ClientError::SessionNotFound(SessionId::new("sess-1"));
        assert_eq!(err.to_string(), "session not found: sess-1");
    }

    #[test]
    fn runtime_error_display_includes_message() {
        let err = ClientError::Runtime("boom".to_string());
        assert_eq!(err.to_string(), "runtime error: boom");
    }
}
