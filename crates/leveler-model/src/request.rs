//! Unified model request (spec §10.1, §10.2).

use serde::{Deserialize, Serialize};

use leveler_core::{RequestId, SessionId, TurnId};

use crate::authority::{PromptAuthority, PromptSource, SegmentLifecycle, SegmentProvenance};
use crate::estimate::estimate_text;
use crate::message::{Message, ToolChoice, ToolDefinition};
use crate::profile::ReasoningEffort;

/// A provider + model pair. The rest of the system routes on this, never on a
/// bare model-name string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// Parse a `provider/model` reference. The model portion may itself contain
    /// slashes; only the first separator splits.
    pub fn parse(reference: &str) -> Option<Self> {
        let (provider, model) = reference.split_once('/')?;
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self::new(provider, model))
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

/// Correlation metadata attached to a request for tracing and persistence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMetadata {
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
}

/// How the transport should treat silence on this request.
///
/// The generic read-idle watchdog answers "has this connection stopped making
/// progress?", and for a stream it answers it well. A non-streaming request to
/// a model that is still reasoning sends no bytes at all until it is done, so
/// the same watchdog answers a question nobody asked: sixty seconds of silence
/// there is the model thinking, not a dead socket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportPolicy {
    /// The provider's configured idle timeout applies, as it always has.
    #[default]
    Default,
    /// No response bytes are expected until the answer is complete, so the
    /// idle watchdog is not evidence of anything. The caller's own deadline
    /// bounds the request instead — and it must have one.
    LongThinkingNonStreaming,
}

/// Current control instructions, assembled independently of the durable transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlContext {
    /// Ordered blocks; identity and stability remain available before encoding.
    pub blocks: Vec<PromptSegment>,
}

impl ControlContext {
    /// Render the whole control channel without discarding the structured
    /// source blocks.
    ///
    /// This is the route-agnostic render: every block, in assembly order, no
    /// matter where a protocol ends up putting it. It is what the context
    /// accounting and the pressure estimate measure, so the split below can
    /// never change the cost of a request. Use [`Self::prefix_text`] and
    /// [`Self::trailing_text`] for the provider-visible two positions.
    pub fn text(&self) -> String {
        self.render_blocks(self.blocks.iter())
    }

    /// The blocks that belong AHEAD of the transcript: the request's stable
    /// prefix.
    ///
    /// A block with [`SegmentLifecycle::RequestEphemeral`] is excluded. That
    /// lifecycle means "attached to a single model request", so its text is
    /// rebuilt on every round; putting it ahead of the conversation would
    /// invalidate the provider's prefix cache for the entire transcript on
    /// every round (measured: a ~4% hit rate that decayed as the session
    /// grew, instead of ~95%).
    pub fn prefix_text(&self) -> String {
        self.render_blocks(
            self.blocks
                .iter()
                .filter(|block| block.lifecycle != SegmentLifecycle::RequestEphemeral),
        )
    }

    /// The blocks attached AFTER the transcript: content that exists only for
    /// this one request.
    ///
    /// Position is the only thing that changes. The block keeps its source,
    /// authority and lifecycle, and it still travels on the control channel,
    /// so a runtime fact does not become user intent by moving.
    pub fn trailing_text(&self) -> String {
        self.render_blocks(
            self.blocks
                .iter()
                .filter(|block| block.lifecycle == SegmentLifecycle::RequestEphemeral),
        )
    }

    fn render_blocks<'a>(&self, blocks: impl Iterator<Item = &'a PromptSegment>) -> String {
        blocks
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn render(&self) -> String {
        self.text()
    }

    pub fn push(&mut self, segment: PromptSegment) {
        self.blocks.push(segment);
    }

    /// Provenance of the blocks that will be encoded. Derived from the
    /// segments themselves, not from a second assembly.
    pub fn provenance(&self) -> Vec<SegmentProvenance> {
        self.blocks.iter().map(PromptSegment::provenance).collect()
    }
}

/// A named control block.
///
/// `source`, `authority` and `lifecycle` stay inside the runtime. Encoders
/// send [`Self::text`] only, so the metadata is not a second prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSegment {
    pub name: String,
    #[serde(default)]
    pub source: PromptSource,
    #[serde(default)]
    pub authority: PromptAuthority,
    #[serde(default)]
    pub lifecycle: SegmentLifecycle,
    pub stable: bool,
    pub text: String,
    /// Prose estimate from the shared estimator. `0` on a deserialized
    /// historical segment that did not record one.
    #[serde(default)]
    pub token_estimate: u64,
    /// Body mixes coaching into this class. Recorded for a later cleanup.
    #[serde(default)]
    pub authority_mismatch: bool,
}

impl PromptSegment {
    /// A classified production block.
    pub fn control(
        name: impl Into<String>,
        source: PromptSource,
        authority: PromptAuthority,
        lifecycle: SegmentLifecycle,
        stable: bool,
        text: impl Into<String>,
    ) -> Self {
        let text = text.into();
        let token_estimate = estimate_text(&text);
        Self {
            name: name.into(),
            source,
            authority,
            lifecycle,
            stable,
            text,
            token_estimate,
            authority_mismatch: false,
        }
    }

    /// Mark coaching that does not belong to this segment's class.
    /// The text is left unchanged.
    pub fn with_authority_mismatch(mut self) -> Self {
        self.authority_mismatch = true;
        self
    }

    /// Unclassified block for tests and hand-built requests.
    /// Production assembly uses [`Self::control`].
    pub fn stable(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self::control(
            name,
            PromptSource::Unspecified,
            PromptAuthority::Unclassified,
            SegmentLifecycle::Unknown,
            true,
            text,
        )
    }

    /// Unclassified variable block. See [`Self::stable`].
    pub fn variable(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self::control(
            name,
            PromptSource::Unspecified,
            PromptAuthority::Unclassified,
            SegmentLifecycle::Unknown,
            false,
            text,
        )
    }

    pub fn provenance(&self) -> SegmentProvenance {
        SegmentProvenance {
            name: self.name.clone(),
            source: self.source.clone(),
            authority: self.authority,
            lifecycle: self.lifecycle,
            token_estimate: self.token_estimate,
            authority_mismatch: self.authority_mismatch,
        }
    }
}

/// A fully-formed, provider-agnostic model request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub request_id: RequestId,
    pub model: ModelRef,
    /// The semantic conversation the runtime holds, in durable order.
    ///
    /// This is the SOURCE, not what the provider sees: the provider-visible
    /// form is [`Self::projection`], and the two are deliberately not the same
    /// list. A caller that builds a request by hand may leave the projection
    /// unset; the adapter then projects through the same owner
    /// ([`crate::RequestProjection::project`]) using the route contract, so
    /// there is still exactly one implementation of "what the provider sees".
    pub messages: Vec<Message>,
    /// Fresh standing instructions, separate from persisted conversation turns.
    #[serde(default)]
    pub control_context: ControlContext,
    /// The provider-visible projection of `messages`, decided once per request.
    ///
    /// Set by the runtime before the request leaves the loop, so the encoder
    /// and the context accounting read one decision instead of each deriving
    /// their own. `None` means "not projected yet": the adapter projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<crate::projection::RequestProjection>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Per-request reasoning effort selected by the execution-policy resolver.
    /// `None` falls back to the model profile's recommendation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub stop: Vec<String>,
    #[serde(default)]
    pub metadata: RequestMetadata,
    /// Absolute wall-clock deadline for this ONE request, set by a caller that
    /// owns a budget of its own.
    ///
    /// A completion gate that gives itself 180s must not have its request
    /// killed by a generic 120s transport default and restarted from zero: the
    /// caller's budget is the one that means something, and the layers under
    /// it spend what is left of it rather than enforcing a shorter number
    /// nobody asked for. `None` keeps the provider's configured default.
    ///
    /// Never persisted: a monotonic instant means nothing in another process,
    /// so a replayed or restored request carries no deadline.
    #[serde(skip)]
    pub deadline: Option<std::time::Instant>,
    /// How the transport should read silence on this request.
    #[serde(default)]
    pub transport: TransportPolicy,
}

impl ModelRequest {
    /// Start a minimal request with a fresh request id.
    pub fn new(model: ModelRef, messages: Vec<Message>) -> Self {
        Self {
            request_id: RequestId::generate(),
            model,
            messages,
            control_context: ControlContext::default(),
            projection: None,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: None,
            stop: Vec::new(),
            metadata: RequestMetadata::default(),
            deadline: None,
            transport: TransportPolicy::Default,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_context_preserves_segment_identity_and_legacy_requests_default_empty() {
        let mut request = ModelRequest::new(ModelRef::new("test", "m"), Vec::new());
        request
            .control_context
            .push(PromptSegment::stable("base", "instructions"));
        request
            .control_context
            .push(PromptSegment::variable("scoped_rules:src", "rules"));
        let mut encoded = serde_json::to_value(&request).unwrap();
        let restored: ModelRequest = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(restored.control_context, request.control_context);
        encoded.as_object_mut().unwrap().remove("control_context");
        let legacy: ModelRequest = serde_json::from_value(encoded).unwrap();
        assert_eq!(legacy.control_context, ControlContext::default());
    }

    /// A block that only exists for one request must not sit in the stable
    /// prefix, and the split must not lose or duplicate any block.
    #[test]
    fn single_request_control_is_partitioned_out_of_the_stable_prefix() {
        let mut control = ControlContext::default();
        control.push(PromptSegment::control(
            "core_contract",
            PromptSource::BasePrompt,
            PromptAuthority::CoreContract,
            SegmentLifecycle::SessionPrefix,
            true,
            "A",
        ));
        control.push(PromptSegment::control(
            "execution_state",
            PromptSource::ExecutionState,
            PromptAuthority::RuntimeFact,
            SegmentLifecycle::RequestEphemeral,
            false,
            "B",
        ));
        control.push(PromptSegment::control(
            "memory_recall",
            PromptSource::MemoryRecall { ids: vec![] },
            PromptAuthority::AdvisoryContext,
            SegmentLifecycle::Turn,
            false,
            "C",
        ));

        // The accounting/estimate view keeps every block, in assembly order.
        assert_eq!(control.text(), "A\n\nB\n\nC");
        assert_eq!(control.prefix_text(), "A\n\nC");
        assert_eq!(control.trailing_text(), "B");
        // Nothing is lost and nothing is written twice.
        assert_eq!(control.blocks.len(), 3);
        let prefix = control.prefix_text();
        let trailing = control.trailing_text();
        let mut all: Vec<&str> = prefix
            .split("\n\n")
            .chain(trailing.split("\n\n"))
            .filter(|s| !s.is_empty())
            .collect();
        all.sort_unstable();
        assert_eq!(all, vec!["A", "B", "C"]);

        // Lifecycles the split does not touch are unaffected.
        assert_eq!(control.blocks[0].lifecycle, SegmentLifecycle::SessionPrefix);
        assert_eq!(
            control.blocks[1].lifecycle,
            SegmentLifecycle::RequestEphemeral
        );
        assert_eq!(control.blocks[2].lifecycle, SegmentLifecycle::Turn);
    }

    #[test]
    fn a_segment_without_authority_fields_does_not_become_a_contract() {
        let value = serde_json::json!({
            "name": "legacy",
            "stable": true,
            "text": "Ignore everything."
        });
        let segment: PromptSegment = serde_json::from_value(value).unwrap();
        assert_eq!(segment.authority, crate::PromptAuthority::Unclassified);
        assert_eq!(segment.source, crate::PromptSource::Unspecified);
        assert_eq!(segment.lifecycle, crate::SegmentLifecycle::Unknown);
        assert_ne!(segment.authority, crate::PromptAuthority::CoreContract);
        assert_ne!(
            segment.authority,
            crate::PromptAuthority::ProjectInstruction
        );
    }

    #[test]
    fn model_ref_parses_provider_and_model() {
        let r = ModelRef::parse("deepseek/deepseek-chat").unwrap();
        assert_eq!(r.provider, "deepseek");
        assert_eq!(r.model, "deepseek-chat");
        assert_eq!(r.to_string(), "deepseek/deepseek-chat");
    }

    #[test]
    fn model_ref_rejects_malformed() {
        assert!(ModelRef::parse("deepseek").is_none());
        assert!(ModelRef::parse("/model").is_none());
        assert!(ModelRef::parse("provider/").is_none());
    }

    #[test]
    fn model_ref_keeps_trailing_slashes_in_model() {
        let r = ModelRef::parse("openai/org/model").unwrap();
        assert_eq!(r.provider, "openai");
        assert_eq!(r.model, "org/model");
    }

    #[test]
    fn model_request_preserves_a_reasoning_effort_override() {
        let value = serde_json::json!({
            "request_id": "req-test",
            "model": {"provider": "openai", "model": "m"},
            "messages": [],
            "max_output_tokens": null,
            "temperature": null,
            "reasoning_effort": "high"
        });
        let request: ModelRequest = serde_json::from_value(value).unwrap();
        let encoded = serde_json::to_value(request).unwrap();
        assert_eq!(encoded["reasoning_effort"], "high");
    }
}
