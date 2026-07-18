//! An in-memory [`InteractiveRuntimeClient`] for tests.
//!
//! It records every command it receives and lets the test push arbitrary
//! [`RuntimeEvent`]s to subscribers — enough to drive a client (e.g. the TUI
//! reducer/event loop) deterministically without a real runtime .

use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::PermissionProfile;
use leveler_core::SessionId;

use super::client::{ClientError, InteractiveRuntimeClient};
use super::command::ClientCommand;
use super::event::RuntimeEvent;
use super::snapshot::UiSessionSnapshot;

/// A scriptable, in-memory runtime client.
pub struct MockRuntimeClient {
    events: broadcast::Sender<RuntimeEvent>,
    commands: Mutex<Vec<ClientCommand>>,
    snapshot: Mutex<UiSessionSnapshot>,
}

impl MockRuntimeClient {
    /// Create a mock seeded with an empty session snapshot.
    pub fn new(session_id: SessionId) -> Self {
        let (events, _) = broadcast::channel(1024);
        let snapshot = UiSessionSnapshot {
            id: session_id,
            repository: "/repo".to_string(),
            goal: "interactive session".to_string(),
            model: None,
            mode: PermissionProfile::Assisted,
            branch: Some("main".to_string()),
            status: "idle".to_string(),
            messages: Vec::new(),
            pending_interactions: Vec::new(),
            available_models: Vec::new(),
            vision: false,
            last_sequence: None,
            active_tools: Vec::new(),
            plan: None,
            verification: None,
            diff: None,
            checkpoints: Vec::new(),
            completion_report: None,
        };
        Self {
            events,
            commands: Mutex::new(Vec::new()),
            snapshot: Mutex::new(snapshot),
        }
    }

    /// Push an event to all current subscribers. Returns the number of
    /// receivers (0 if none, which is fine).
    pub fn emit(&self, event: RuntimeEvent) {
        let _ = self.events.send(event);
    }

    /// Every command the client has received so far, in order.
    pub fn commands(&self) -> Vec<ClientCommand> {
        self.commands.lock().unwrap().clone()
    }

    /// Replace the snapshot returned by [`InteractiveRuntimeClient::snapshot`].
    pub fn set_snapshot(&self, snapshot: UiSessionSnapshot) {
        *self.snapshot.lock().unwrap() = snapshot;
    }
}

#[async_trait]
impl InteractiveRuntimeClient for MockRuntimeClient {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        self.commands.lock().unwrap().push(command);
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    async fn snapshot(&self, _session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        Ok(self.snapshot.lock().unwrap().clone())
    }
}
