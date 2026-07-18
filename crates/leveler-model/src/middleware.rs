//! Provider compatibility middleware (spec §14).
//!
//! "OpenAI compatible" rarely means identical behavior. Middleware patches the
//! request before encoding and events/responses after decoding, so per-provider
//! quirks never leak into the agent core.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::event::ModelEvent;
use crate::profile::CompatibilityConfig;
use crate::request::ModelRequest;
use crate::response::ModelResponse;

/// Context available to middleware (currently the compatibility config; grows
/// as needed).
#[derive(Debug, Clone)]
pub struct CompatibilityContext {
    pub config: CompatibilityConfig,
}

/// A middleware failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("compatibility middleware error: {0}")]
pub struct CompatibilityError(pub String);

/// Hooks that run around protocol encode/decode to smooth over provider quirks.
#[async_trait]
pub trait CompatibilityMiddleware: Send + Sync {
    /// A stable name (used to select middleware from a profile).
    fn name(&self) -> &'static str;

    /// Mutate the unified request before it is encoded.
    async fn before_encode(
        &self,
        _request: &mut ModelRequest,
        _context: &CompatibilityContext,
    ) -> Result<(), CompatibilityError> {
        Ok(())
    }

    /// Mutate each decoded event (e.g. synthesize a missing tool-call id).
    async fn after_event(
        &self,
        _event: &mut ModelEvent,
        _context: &CompatibilityContext,
    ) -> Result<(), CompatibilityError> {
        Ok(())
    }

    /// Mutate a decoded non-streaming response.
    async fn after_response(
        &self,
        _response: &mut ModelResponse,
        _context: &CompatibilityContext,
    ) -> Result<(), CompatibilityError> {
        Ok(())
    }
}
