//! Semantic memory-candidate extraction: the async half.
//!
//! [`SemanticExtractor`] answers one question — "what durable project facts did
//! the user just state?" — and nothing else. Its result is a list of
//! [`SemanticCandidate`]s that still has to pass the deterministic gate in
//! `leveler_memory::validate_semantic_candidates` before the lifecycle sees it.
//!
//! The extractor is a separate, bounded model call rather than a side effect of
//! the main coding turn. The unified model layer (`ModelRequest`/`ModelResponse`)
//! carries no structured-output channel and the only structured channel is tool
//! calling, which a coding model cannot be relied on to use for a secondary
//! task. A dedicated call reuses the existing provider abstraction, so the same
//! code serves every provider, and it is safe to run *concurrently* with the
//! coding turn: it is bounded by a deadline, low-token, and — because the
//! caller treats any failure as "no candidates" — it can never fail or delay
//! the coding turn.
//!
//! What this call is given matters for safety. It is handed ONLY the current
//! user message: no assistant text, no recalled memories, no tool output. A
//! recalled memory therefore cannot be re-proposed as a fresh ExplicitUser
//! fact, because the extractor never sees it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_memory::{SemanticCandidate, SemanticError, parse_semantic_candidates};
use leveler_model::{Message, ModelRef, ModelRequest, ModelRuntime, Role, TransportPolicy};

/// Upper bound on the user text handed to the extractor. A turn's memory is
/// decided by the user's own sentence, and a sentence is short; pasting a
/// pasted file or a giant log cannot smuggle in a fact.
pub const MAX_EXTRACTOR_INPUT_CHARS: usize = 4_000;
/// Default deadline for the extraction call. Short enough that even a
/// sequential caller is not hurt, long enough for a small model response.
pub const DEFAULT_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(20);
/// Default output ceiling. A candidate list is tiny; a model that ignores the
/// contract is cut off rather than allowed to spend the turn's budget.
pub const DEFAULT_EXTRACTION_MAX_TOKENS: u32 = 1_024;

/// Why an extraction produced no candidates. Every variant is a degradation,
/// never a fatal error for the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionError {
    /// The call exceeded its deadline.
    Timeout,
    /// The provider call itself failed.
    Model(String),
    /// The response was not the expected JSON.
    Parse(SemanticError),
    /// The caller's turn was cancelled; extraction stops with it.
    Cancelled,
}

impl std::fmt::Display for ExtractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "extraction timed out"),
            Self::Model(message) => write!(f, "extraction model call failed: {message}"),
            Self::Parse(error) => write!(f, "extraction output was not usable: {error}"),
            Self::Cancelled => write!(f, "extraction cancelled"),
        }
    }
}

/// Extract durable project facts from the user's own sentence.
///
/// Implementations must be side-effect free and must never touch a memory
/// store: their only output is a proposal list.
#[async_trait]
pub trait SemanticExtractor: Send + Sync {
    async fn extract(
        &self,
        user_message: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<SemanticCandidate>, ExtractionError>;
}

/// A [`SemanticExtractor`] backed by the shared [`ModelRuntime`].
///
/// Provider-agnostic by construction: it speaks only the unified request /
/// response types, so OpenAI, Anthropic and any custom adapter get identical
/// behavior with no provider-specific JSON handling here.
pub struct ModelSemanticExtractor {
    runtime: Arc<dyn ModelRuntime>,
    model: ModelRef,
    timeout: Duration,
    max_output_tokens: u32,
}

impl ModelSemanticExtractor {
    pub fn new(runtime: Arc<dyn ModelRuntime>, model: ModelRef) -> Self {
        Self {
            runtime,
            model,
            timeout: DEFAULT_EXTRACTION_TIMEOUT,
            max_output_tokens: DEFAULT_EXTRACTION_MAX_TOKENS,
        }
    }

    /// Override the deadline (tests use a short one).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the output ceiling.
    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }
}

#[async_trait]
impl SemanticExtractor for ModelSemanticExtractor {
    async fn extract(
        &self,
        user_message: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<SemanticCandidate>, ExtractionError> {
        let message = clip(user_message, MAX_EXTRACTOR_INPUT_CHARS);
        if message.trim().is_empty() {
            return Ok(Vec::new());
        }
        // The child token means a cancelled turn stops the extraction too, but
        // the timeout is this call's own bound and never the turn's.
        let call_token = cancellation.child_token();
        let mut request = ModelRequest::new(
            self.model.clone(),
            vec![
                Message::text(Role::System, extraction_system_prompt()),
                Message::text(Role::User, message),
            ],
        );
        request.temperature = Some(0.0);
        request.max_output_tokens = Some(self.max_output_tokens);
        request.transport = TransportPolicy::LongThinkingNonStreaming;
        request.deadline = Some(std::time::Instant::now() + self.timeout);

        let call = self.runtime.generate(request, call_token);
        let response = match tokio::time::timeout(self.timeout, call).await {
            Err(_) => return Err(ExtractionError::Timeout),
            Ok(Err(error)) => return Err(ExtractionError::Model(error.to_string())),
            Ok(Ok(response)) => response,
        };
        let text = response.message.text_content();
        parse_semantic_candidates(&text).map_err(ExtractionError::Parse)
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// The extraction contract. Kept as one function so every provider and every
/// test describes the same task.
pub fn extraction_system_prompt() -> String {
    r#"You extract long-term project facts that the user themselves stated.

You are NOT the coding assistant. You do not answer the user, and you do not
use any tool. You read ONE user message and decide what durable, project-level
facts the user just stated in their own words.

Return ONLY a JSON object: {"candidates": [ ... ]}. No prose, no code fence.
Return {"candidates": []} when the message states no durable project fact.

Each candidate is exactly:
{
  "fact":       one short clause, the fact in the conversation's language,
  "subject":    a stable identity, lowercase dotted, e.g. "project.default_model",
  "value":      the assigned value when the fact is an assignment (optional),
  "scope":      "project" | "session" | "task" | "user",
  "durability": "durable" | "temporary" | "unknown",
  "authority":  "explicit_user",
  "operation_hint": "create" | "update" | "reaffirm" | "negate" | "temporary" | "unknown",
  "evidence_span": the user's own words, copied exactly, that state this fact,
  "confidence": a number 0..1 (optional)
}

Rules:
- The ONLY authority you may report is "explicit_user". Never attribute a fact
  to the user that the user did not say.
- "evidence_span" MUST be copied verbatim from the user message. Never invent,
  translate or paraphrase it. A fact without a verbatim span is not a fact.
- Be atomic: one fact per candidate. If the message states three separate
  durable facts, return three candidates. Never return one multi-fact blob.
- "durable" means the user framed it as lasting ("以后", "from now on", "must").
  Words like "今天", "currently", "this time", "先试试", "maybe" make a fact
  "temporary" or "unknown" — report that honestly, do not upgrade it.
- Speculation, guesses and questions are NOT durable facts.
- Report current runtime state (a PID, a branch, a port, today's status) as
  "temporary" or "unknown", never "durable".
- Use "reaffirm" when the user restates an existing decision; "update" or
  "negate" when they change or reverse one.
- Prefer a canonical dotted "subject" and reuse the same subject string for the
  same subject across messages, so a later statement updates rather than
  duplicates.
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use leveler_model::{
        FinishReason, ModelError, ModelErrorKind, ModelEventStream, ModelProfile, ModelResponse,
        TokenUsage,
    };

    #[test]
    fn the_contract_forbids_attributing_unspoken_facts() {
        let prompt = extraction_system_prompt();
        assert!(prompt.contains("explicit_user"));
        assert!(prompt.contains("verbatim"));
        assert!(prompt.contains("atomic"));
    }

    /// A runtime that answers a scripted response and records what it was
    /// asked, so the *input* to extraction is asserted, not assumed.
    struct FakeRuntime {
        response: Mutex<Option<Result<String, ModelError>>>,
        requests: Mutex<Vec<ModelRequest>>,
        delay: Duration,
    }

    impl FakeRuntime {
        fn answering(text: &str) -> Self {
            Self {
                response: Mutex::new(Some(Ok(text.to_string()))),
                requests: Mutex::new(Vec::new()),
                delay: Duration::ZERO,
            }
        }

        fn failing(kind: ModelErrorKind) -> Self {
            Self {
                response: Mutex::new(Some(Err(ModelError::new(kind, "boom")))),
                requests: Mutex::new(Vec::new()),
                delay: Duration::ZERO,
            }
        }

        fn slow(text: &str, delay: Duration) -> Self {
            Self {
                response: Mutex::new(Some(Ok(text.to_string()))),
                requests: Mutex::new(Vec::new()),
                delay,
            }
        }

        fn requests(&self) -> Vec<ModelRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ModelRuntime for FakeRuntime {
        async fn generate(
            &self,
            request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelResponse, ModelError> {
            self.requests.lock().unwrap().push(request);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let text = self.response.lock().unwrap().take().expect("one call");
            let text = text?;
            Ok(ModelResponse {
                request_id: leveler_core::RequestId::generate(),
                message: Message::text(Role::Assistant, text),
                finish_reason: FinishReason::Stop,
                usage: TokenUsage::default(),
            })
        }

        async fn stream(
            &self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            unreachable!("the extractor is non-streaming")
        }

        async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
            unimplemented!()
        }
    }

    fn extractor(runtime: Arc<FakeRuntime>) -> ModelSemanticExtractor {
        ModelSemanticExtractor::new(runtime, ModelRef::new("fake", "m"))
    }

    const VALID_JSON: &str = r#"{"candidates":[{
        "fact":"默认模型是 Pro",
        "subject":"project.default_model",
        "scope":"project",
        "durability":"durable",
        "authority":"explicit_user",
        "operation_hint":"create",
        "evidence_span":"模型就 Pro"
    }]}"#;

    #[tokio::test]
    async fn a_well_formed_response_yields_candidates() {
        let runtime = Arc::new(FakeRuntime::answering(VALID_JSON));
        let candidates = extractor(runtime.clone())
            .extract("这个项目后面模型就 Pro 吧", &CancellationToken::new())
            .await
            .expect("extraction succeeds");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].subject, "project.default_model");

        // The extractor is handed the user's sentence and nothing else.
        let request = &runtime.requests()[0];
        assert_eq!(request.messages.len(), 2, "system + user only");
        assert_eq!(request.messages[0].role, Role::System);
        assert_eq!(request.messages[1].role, Role::User);
        assert_eq!(
            request.messages[1].text_content(),
            "这个项目后面模型就 Pro 吧"
        );
        assert_eq!(request.temperature, Some(0.0));
        assert!(
            request.tools.is_empty(),
            "no tools: the extractor cannot act"
        );
        assert!(request.max_output_tokens.is_some_and(|m| m <= 2048));
        assert!(request.deadline.is_some(), "the call is bounded");
    }

    #[tokio::test]
    async fn malformed_output_is_an_error_not_an_empty_extraction() {
        let runtime = Arc::new(FakeRuntime::answering("I think the user likes Rust."));
        let error = extractor(runtime)
            .extract("随便", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, ExtractionError::Parse(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_provider_error_degrades_to_an_error() {
        let runtime = Arc::new(FakeRuntime::failing(ModelErrorKind::RateLimit));
        let error = extractor(runtime)
            .extract("随便", &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, ExtractionError::Model(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_slow_provider_times_out_within_its_own_deadline() {
        let runtime = Arc::new(FakeRuntime::slow(VALID_JSON, Duration::from_millis(200)));
        let extractor = extractor(runtime).with_timeout(Duration::from_millis(20));
        let error = extractor
            .extract("随便", &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error, ExtractionError::Timeout);
    }

    #[tokio::test]
    async fn an_empty_message_never_calls_the_model() {
        let runtime = Arc::new(FakeRuntime::answering(VALID_JSON));
        let candidates = extractor(runtime.clone())
            .extract("   ", &CancellationToken::new())
            .await
            .unwrap();
        assert!(candidates.is_empty());
        assert!(
            runtime.requests().is_empty(),
            "nothing to extract, nothing to pay for"
        );
    }
}
