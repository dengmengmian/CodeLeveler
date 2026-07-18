use tokio_util::sync::CancellationToken;

use leveler_model::{
    ContentPart, Message, ModelError, ModelRequest, Role, TokenUsage, ToolCall, ToolChoice,
};

use super::{ADVISORY_REQUEST_TIMEOUT, AgentError, AgentEvent, Executor, StreamRoundResult};
use crate::compaction::{COMPACT_PROMPT, compaction_span};

impl Executor {
    /// Run one model round over a stream and assemble the final assistant
    /// message (spec §16). Each attempt starts with an explicit reset event so
    /// retries can stream a divergent prefix without corrupting consumers.
    /// Stream a round, retrying the SAME request from history on a retryable
    /// error (rate-limit, timeout, mid-stream interruption). The provider's own
    /// retry only covers connection setup; once bytes flow, a dropped stream
    /// would otherwise abort the whole task and lose all progress. Non-retryable
    /// errors and cancellation propagate immediately.
    pub(crate) async fn stream_round_with_retry(
        &self,
        request: ModelRequest,
        observer: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<StreamRoundResult, AgentError> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0u32;
        let started = std::time::Instant::now();
        loop {
            attempt += 1;
            observer(AgentEvent::StreamAttemptStarted);
            match self
                .stream_round(request.clone(), observer, cancellation)
                .await
            {
                Ok(mut value) => {
                    value.retry_count = attempt.saturating_sub(1);
                    value.latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    return Ok(value);
                }
                Err(AgentError::Model(e))
                    if e.retryable && attempt < MAX_ATTEMPTS && !cancellation.is_cancelled() =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64))
                        .await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Stream one model round, preserving the provider's terminal reason. A
    /// stream that ends without a terminal event is never treated as success.
    async fn stream_round(
        &self,
        request: ModelRequest,
        observer: &mut dyn FnMut(AgentEvent),
        cancellation: &CancellationToken,
    ) -> Result<StreamRoundResult, AgentError> {
        use futures::StreamExt;
        use leveler_model::ModelEvent;

        let request_id = request.request_id.as_str().to_string();
        let mut usage = TokenUsage::default();
        let mut finish_reason = None;

        let mut stream = match self
            .runtime
            .stream(request, cancellation.child_token())
            .await
        {
            Ok(s) => s,
            Err(e)
                if cancellation.is_cancelled()
                    || e.kind == leveler_model::ModelErrorKind::Cancelled =>
            {
                return Err(AgentError::Cancelled);
            }
            Err(e) => return Err(AgentError::Model(e)),
        };

        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        // Some providers (and some gateways) emit `usage` in a chunk *after*
        // `finish_reason`. Breaking at MessageCompleted drops that chunk and the
        // TUI context gauge stays stuck at "— / window". After completion we
        // only accept UsageUpdated / terminal errors until the stream ends.
        let mut completed = false;

        while let Some(event) = stream.next().await {
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            match event {
                Ok(ModelEvent::TextDelta { delta }) if !completed => {
                    if !delta.is_empty() {
                        text.push_str(&delta);
                        observer(AgentEvent::AssistantDelta(delta));
                    }
                }
                Ok(ModelEvent::ReasoningDelta { delta }) if !completed => {
                    if !delta.is_empty() {
                        observer(AgentEvent::ReasoningDelta(delta));
                    }
                }
                Ok(ModelEvent::ToolCallCompleted { call }) if !completed => calls.push(call),
                Ok(ModelEvent::MessageCompleted {
                    finish_reason: reason,
                }) => {
                    finish_reason = Some(reason);
                    completed = true;
                }
                Ok(ModelEvent::Error { error }) => return Err(AgentError::Model(error)),
                Ok(ModelEvent::UsageUpdated {
                    usage: latest_usage,
                }) => {
                    observer(AgentEvent::Usage {
                        input_tokens: latest_usage.input_tokens.min(u32::MAX as u64) as u32,
                        output_tokens: latest_usage.output_tokens.min(u32::MAX as u64) as u32,
                        cached_input_tokens: latest_usage.cached_input_tokens.min(u32::MAX as u64)
                            as u32,
                    });
                    usage = latest_usage;
                }
                // Reasoning / start / partial tool-call fragments, or content
                // after completion — no assembly state we need here.
                Ok(_) => {}
                Err(e)
                    if cancellation.is_cancelled()
                        || e.kind == leveler_model::ModelErrorKind::Cancelled =>
                {
                    return Err(AgentError::Cancelled);
                }
                Err(e) => return Err(AgentError::Model(e)),
            }
        }

        let finish_reason = finish_reason.ok_or_else(|| {
            AgentError::Model(ModelError::new(
                leveler_model::ModelErrorKind::StreamInterrupted,
                "model stream ended without a terminal completion event",
            ))
        })?;

        // Guarantee one final usage signal for the UI even when the provider
        // only reported tokens once mid-stream (or only after completion).
        if usage.total() > 0 {
            observer(AgentEvent::Usage {
                input_tokens: usage.input_tokens.min(u32::MAX as u64) as u32,
                output_tokens: usage.output_tokens.min(u32::MAX as u64) as u32,
                cached_input_tokens: usage.cached_input_tokens.min(u32::MAX as u64) as u32,
            });
        }

        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentPart::Text { text });
        }
        for call in calls {
            content.push(ContentPart::ToolCall { call });
        }
        Ok(StreamRoundResult {
            request_id,
            message: Message {
                role: Role::Assistant,
                content,
            },
            usage,
            finish_reason,
            latency_ms: 0,
            retry_count: 0,
        })
    }

    /// Ask the model to write a handoff briefing for the rounds compaction is
    /// about to elide. Returns None when there is nothing to fold or the call
    /// fails — the caller then folds with a bare breadcrumb rather than aborting
    /// the run, because an unsummarized fold still beats overflowing the window.
    /// The breadcrumb says the details are lost, so the loss is never silent.
    pub(crate) async fn summarize_for_compaction(
        &self,
        messages: &[Message],
        keep_recent: usize,
        cancellation: &CancellationToken,
    ) -> Option<String> {
        let (_, tail_start) = compaction_span(messages, keep_recent)?;
        let mut summary_messages = messages[..tail_start].to_vec();
        summary_messages.push(Message::text(Role::User, COMPACT_PROMPT));

        let mut request = ModelRequest::new(self.model.clone(), summary_messages);
        request.tool_choice = ToolChoice::None;
        request.max_output_tokens = Some(1024);
        request.reasoning_effort = self.reasoning_effort;

        let response = tokio::time::timeout(
            ADVISORY_REQUEST_TIMEOUT,
            self.runtime.generate(request, cancellation.child_token()),
        )
        .await
        .ok()?
        .ok()?;
        let text = response.message.text_content().trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}
