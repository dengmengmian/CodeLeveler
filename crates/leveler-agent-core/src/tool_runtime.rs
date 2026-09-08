//! The kernel's tool boundary.
//!
//! The kernel knows nothing about how a tool is authorized, sandboxed, or
//! run. It needs exactly this much: the definitions the model may see, and a
//! way to turn a [`ToolCall`] into text the model reads next. Everything
//! behind [`ToolRuntime::execute`] — admission, permission, workspace,
//! side-effect durability — belongs to the host.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_model::{ContentPart, Message, Role, ToolCall, ToolDefinition, ToolResultContent};

use crate::event::{AgentEvent, preview};

/// What one tool call produced, as the model will read it. `is_error` marks
/// a result the model should treat as a failure; it is still a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// The tool runtime itself failed, as opposed to a tool returning an error
/// result. This aborts the run: an unrecorded side effect is not something
/// the model can be asked to reason about.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ToolRuntimeError {
    pub message: String,
}

impl ToolRuntimeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A host's tool execution boundary, one call at a time.
#[async_trait]
pub trait ToolRuntime: Send + Sync {
    /// The tools the model may call, as the request will advertise them.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// Execute one call. A refused, unknown, or failed tool is an error
    /// *result* (`is_error: true`) the model reads; `Err` is reserved for the
    /// runtime failing to produce any result at all.
    async fn execute(
        &self,
        call: ToolCall,
        cancellation: CancellationToken,
    ) -> Result<ToolOutcome, ToolRuntimeError>;
}

/// Execute a round's calls in order, one at a time, and fold the results into
/// the single tool message the model receives next. Results stay in call
/// order, which providers require.
pub async fn dispatch_calls(
    tools: &dyn ToolRuntime,
    calls: Vec<ToolCall>,
    cancellation: &CancellationToken,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
) -> Result<Message, ToolRuntimeError> {
    let mut results = Vec::with_capacity(calls.len());
    for call in calls {
        on_event(AgentEvent::ToolCallStarted {
            id: call.id.as_str().to_string(),
            name: call.name.clone(),
            arguments: preview(&call.arguments.to_string()),
        });
        let id = call.id.clone();
        let name = call.name.clone();
        let outcome = if cancellation.is_cancelled() {
            ToolOutcome::error("cancelled before this call ran")
        } else {
            tools.execute(call, cancellation.child_token()).await?
        };
        on_event(AgentEvent::ToolCallFinished {
            id: id.as_str().to_string(),
            name,
            is_error: outcome.is_error,
            preview: preview(&outcome.content),
        });
        results.push(ContentPart::ToolResult {
            result: ToolResultContent {
                call_id: id,
                content: outcome.content,
                is_error: outcome.is_error,
            },
        });
    }
    Ok(Message {
        role: Role::Tool,
        content: results,
    })
}
