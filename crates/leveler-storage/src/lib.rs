//! `leveler-storage` — SQLite persistence.
//!
//! Owns the connection pool, embedded migrations, and repositories. Business
//! logic never issues SQL directly; it goes through a repository (spec §8.15).
//! Migrations are embedded at compile time so no `DATABASE_URL` is required to
//! build (spec §6.7 offline note).
#![forbid(unsafe_code)]

mod command_receipt_repo;
mod database;
mod event_repo;
mod event_store;
mod message_repo;
mod model_request_repo;
mod session_repo;
mod terminal_repo;
mod turn_repo;

pub use command_receipt_repo::{Admission, CommandReceiptRepository};
pub use database::{Database, StorageError};
pub use event_repo::{EVENT_SCHEMA_VERSION, EventRecord, EventRepository};
pub use event_store::{EventStore, MemoryEventStore};
pub use message_repo::MessageRepository;
pub use model_request_repo::{ModelRequestRecord, ModelRequestRepository};
pub use session_repo::{SessionRecord, SessionRepository};
pub use terminal_repo::TerminalRepository;
pub use turn_repo::{TurnRecord, TurnRepository};
