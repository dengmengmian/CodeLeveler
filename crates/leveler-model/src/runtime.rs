//! The `ModelRuntime` trait — the single seam the agent uses to talk to models
//! (spec §12). Concrete implementations live in `leveler-provider`.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use tokio_util::sync::CancellationToken;

use crate::error::ModelError;
use crate::event::ModelEvent;
use crate::profile::ModelProfile;
use crate::request::{ModelRef, ModelRequest};
use crate::response::ModelResponse;

/// A pinned, boxed stream of normalized model events.
pub type ModelEventStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ModelError>> + Send>>;

/// Everything the agent needs from a model, expressed model-agnostically.
#[async_trait]
pub trait ModelRuntime: Send + Sync {
    /// Stream a response as normalized [`ModelEvent`]s.
    async fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError>;

    /// Produce a fully-assembled non-streaming response.
    async fn generate(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError>;

    /// The capability profile for a model.
    async fn profile(&self, model: &ModelRef) -> Result<ModelProfile, ModelError>;
}
