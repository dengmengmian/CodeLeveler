//! Explicit client-observed metadata versions for legacy behavioral fixtures.
use leveler_client_protocol::{
    ClientCommand, ClientError, CommandEnvelope, InteractiveRuntimeClient,
};

pub trait ObservedSettings: InteractiveRuntimeClient {
    async fn send_observed(&self, command: ClientCommand) -> Result<(), ClientError> {
        let session_id = command
            .session_id()
            .expect("session-scoped setting")
            .clone();
        let observed = self.snapshot(&session_id).await?;
        self.deliver(CommandEnvelope {
            command_id: leveler_client_protocol::CommandId::generate(),
            session_id,
            expected_version: Some(observed.last_sequence.unwrap_or(0)),
            issued_at: leveler_core::now().to_rfc3339(),
            command,
        })
        .await
    }
}
impl<T: InteractiveRuntimeClient + ?Sized> ObservedSettings for T {}
