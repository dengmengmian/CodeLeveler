//! `leveler-storage` — SQLite persistence.
//!
//! Owns the connection pool, embedded migrations, and repositories. Business
//! logic never issues SQL directly; it goes through a repository (spec §8.15).
//! Migrations are embedded at compile time so no `DATABASE_URL` is required to
//! build (spec §6.7 offline note).
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod command_receipt_repo;
mod database;
mod engine_stores;
mod event_repo;
mod event_store;
mod message_repo;
mod message_store;
mod model_request_repo;
mod ownership_store;
mod session_repo;
mod session_store;
mod task_store;
mod terminal_repo;
mod terminal_store;
mod turn_repo;
mod turn_store;

pub use command_receipt_repo::{Admission, CommandReceiptRepository};
pub use database::{Database, StorageError, peek_repository};
pub use engine_stores::EngineStores;
pub use event_repo::{EVENT_SCHEMA_VERSION, EventRecord, EventRepository};
pub use event_store::{EventStore, MemoryEventStore};
pub use message_repo::MessageRepository;
pub use message_store::{
    MemoryMessageStore, MemoryModelRequestStore, MessageStore, ModelRequestStore,
};
pub use model_request_repo::{ModelRequestRecord, ModelRequestRepository};
pub use ownership_store::{
    MemoryOwnershipState, MemoryOwnershipStore, OwnershipError, OwnershipStore, TaskOwner,
};
pub use session_repo::{SessionRecord, SessionRepository};
pub use session_store::{MemorySessionStore, SessionStore};
pub use task_store::{MemoryTaskStore, TaskStore};
pub use terminal_repo::TerminalRepository;
pub use terminal_store::{MemoryTerminalStore, TerminalStore};
pub use turn_repo::{TurnRecord, TurnRepository};
pub use turn_store::{MemoryTurnStore, TurnStore};

/// Redact secrets in a durable JSON payload without touching its structure,
/// refusing loudly when the input is not valid JSON (R007 F2).
///
/// Every writer of a JSON-contract column (events, session messages, turn
/// payloads) must go through this boundary: the serialized-text scrubber used
/// to swallow JSON structural bytes and persist unreplayable rows. `context`
/// names the plane for the error message and must never contain payload bytes.
pub(crate) fn redact_json_payload(context: &str, payload: &str) -> Result<String, StorageError> {
    leveler_core::redact_secrets_json(payload).map_err(|e| {
        StorageError::InvalidData(format!(
            "refusing to persist non-JSON {context} payload: {e}"
        ))
    })
}
