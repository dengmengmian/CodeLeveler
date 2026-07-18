//! Drives a multi-agent discussion with a scripted mock runtime.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_core::RequestId;
use leveler_model::{
    FinishReason, Message, ModelError, ModelEventStream, ModelProfile, ModelRef, ModelRequest,
    ModelResponse, ModelRuntime, Role, TokenUsage,
};
use leveler_orchestrator::{Discussion, DiscussionEvent, Participant};

struct MockRuntime {
    responses: Mutex<VecDeque<String>>,
}

#[async_trait]
impl ModelRuntime for MockRuntime {
    async fn generate(
        &self,
        _r: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let text = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "…".into());
        Ok(ModelResponse {
            request_id: RequestId::generate(),
            message: Message::text(Role::Assistant, text),
            finish_reason: FinishReason::Stop,
            usage: TokenUsage::default(),
        })
    }
    async fn stream(
        &self,
        _r: ModelRequest,
        _c: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        unimplemented!()
    }
    async fn profile(&self, _m: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

#[tokio::test]
async fn discussion_runs_rounds_then_synthesizes() {
    // 2 participants × 2 rounds = 4 turns, then 1 synthesis = 5 responses.
    let runtime = Arc::new(MockRuntime {
        responses: Mutex::new(VecDeque::from(vec![
            "A-r1".into(),
            "B-r1".into(),
            "A-r2".into(),
            "B-r2".into(),
            "CONCLUSION: ship it".into(),
        ])),
    });

    let discussion = Discussion::new(runtime, ModelRef::new("mock", "m"))
        .with_participants(vec![
            Participant {
                name: "A".into(),
                persona: "a".into(),
            },
            Participant {
                name: "B".into(),
                persona: "b".into(),
            },
        ])
        .with_rounds(2);

    let mut events = Vec::new();
    let outcome = discussion
        .run(
            "how to cache",
            &mut |e| events.push(e),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.transcript.len(), 4);
    assert_eq!(outcome.transcript[0].speaker, "A");
    assert_eq!(outcome.transcript[3].speaker, "B");
    assert!(outcome.synthesis.contains("ship it"));

    let synth_events = events
        .iter()
        .filter(|e| matches!(e, DiscussionEvent::Synthesis(_)))
        .count();
    assert_eq!(synth_events, 1);
}
