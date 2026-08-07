//! The engine's persistence dependency bundle.
//!
//! A plain composition struct — NOT a trait — aggregating the narrow ports
//! the engine needs. Each field stays an independent capability; bundling
//! them changes nothing about their contracts, it only keeps constructor
//! signatures sane. The SQLite constructor lives here so the composition
//! root (and tests) wire the production adapter in one line while the engine
//! itself never names `Database`.

use std::sync::Arc;

use crate::{
    Database, EventStore, MessageStore, ModelRequestStore, SessionStore, TaskStore, TerminalStore,
    TurnStore,
};

/// Every persistence port the engine consumes. Cloning is cheap (Arc per
/// field); one bundle is built at composition time and shared.
#[derive(Clone)]
pub struct EngineStores {
    /// Canonical append-only event log.
    pub events: Arc<dyn EventStore>,
    /// Durable task identity and task↔session association.
    pub tasks: Arc<dyn TaskStore>,
    /// Session rows: create, execution config, the Running transition.
    pub sessions: Arc<dyn SessionStore>,
    /// Turn rows: start + the running-turn recovery query.
    pub turns: Arc<dyn TurnStore>,
    /// Transcript payloads: turn-stamped append + whole-session load.
    pub messages: Arc<dyn MessageStore>,
    /// Model-call telemetry rows.
    pub model_requests: Arc<dyn ModelRequestStore>,
    /// Atomic terminal commits (event + projection in one transaction).
    pub terminal: Arc<dyn TerminalStore>,
    /// Durable task ownership (CAS acquire + current-owner reads).
    pub ownership: Arc<dyn crate::OwnershipStore>,
}

impl EngineStores {
    /// The production wiring: every port backed by the same SQLite database
    /// (shared connection pool; the clones are pool handles, not new pools).
    pub fn from_database(db: &Database) -> Self {
        Self {
            events: Arc::new(db.clone()),
            tasks: Arc::new(db.clone()),
            sessions: Arc::new(db.clone()),
            turns: Arc::new(db.clone()),
            messages: Arc::new(db.clone()),
            model_requests: Arc::new(db.clone()),
            terminal: Arc::new(db.clone()),
            ownership: Arc::new(db.clone()),
        }
    }
}
