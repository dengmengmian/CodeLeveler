//! Basic model probing (spec §41, §53.17): exercise text and streaming paths so
//! `leveler model probe` can report whether a newly-configured model actually
//! works. The full 10-task probe with a recommended policy is a later phase.

use tokio_util::sync::CancellationToken;

use leveler_model::{
    ContentPart, FinishReason, Message, ModelEvent, ModelRef, ModelRequest, ModelRuntime, Role,
    TokenUsage,
};

use futures::StreamExt;

/// The outcome of a basic probe.
#[derive(Debug, Clone, Default)]
pub struct BasicProbeReport {
    /// Non-streaming generate returned assistant text.
    pub text_ok: bool,
    /// Streaming produced a clean completion (no error event).
    pub stream_ok: bool,
    /// The text assembled from the streaming run.
    pub streamed_text: String,
    /// Reasoning text assembled from the streaming run (for reasoning models).
    pub streamed_reasoning: String,
    /// Token usage from the streaming run, if reported.
    pub usage: TokenUsage,
    /// The streaming finish reason, if the stream completed.
    pub finish_reason: Option<FinishReason>,
    /// First error encountered, if any.
    pub error: Option<String>,
}

impl BasicProbeReport {
    /// Whether both probe paths succeeded.
    pub fn healthy(&self) -> bool {
        self.text_ok && self.stream_ok && self.error.is_none()
    }
}

/// Whether a message carries any non-empty assistant output — text *or*
/// reasoning. Reasoning models legitimately answer with only reasoning content.
fn has_output(message: &Message) -> bool {
    message.content.iter().any(|part| match part {
        ContentPart::Text { text } | ContentPart::Reasoning { text } => !text.trim().is_empty(),
        _ => false,
    })
}

fn probe_request(model: &ModelRef) -> ModelRequest {
    let mut req = ModelRequest::new(
        model.clone(),
        vec![
            Message::text(Role::System, "You are a health probe. Answer concisely."),
            Message::text(Role::User, "Reply with the single word: OK"),
        ],
    );
    // Generous enough for a reasoning model to emit visible content after its
    // internal reasoning, rather than exhausting the budget mid-thought.
    req.max_output_tokens = Some(256);
    req.temperature = Some(0.0);
    req
}

/// Run the basic text + streaming probe against a runtime.
pub async fn probe_basic(
    runtime: &dyn ModelRuntime,
    model: &ModelRef,
    cancellation: CancellationToken,
) -> BasicProbeReport {
    let mut report = BasicProbeReport::default();

    // 1. Non-streaming text.
    match runtime
        .generate(probe_request(model), cancellation.clone())
        .await
    {
        Ok(resp) => {
            report.text_ok = has_output(&resp.message);
        }
        Err(e) => {
            report.error.get_or_insert(e.to_string());
        }
    }

    // 2. Streaming text.
    match runtime
        .stream(probe_request(model), cancellation.clone())
        .await
    {
        Ok(mut stream) => {
            let mut completed = false;
            while let Some(event) = stream.next().await {
                match event {
                    Ok(ModelEvent::TextDelta { delta }) => report.streamed_text.push_str(&delta),
                    Ok(ModelEvent::ReasoningDelta { delta }) => {
                        report.streamed_reasoning.push_str(&delta)
                    }
                    Ok(ModelEvent::UsageUpdated { usage }) => report.usage = usage,
                    Ok(ModelEvent::MessageCompleted { finish_reason }) => {
                        report.finish_reason = Some(finish_reason);
                        completed = true;
                    }
                    Ok(ModelEvent::Error { error }) => {
                        report.error.get_or_insert(error.to_string());
                    }
                    _ => {}
                }
            }
            report.stream_ok = completed && report.error.is_none();
        }
        Err(e) => {
            report.error.get_or_insert(e.to_string());
        }
    }

    report
}
