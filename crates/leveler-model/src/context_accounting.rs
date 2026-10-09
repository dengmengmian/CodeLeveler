//! Context accounting: a deterministic projection of what one assembled model
//! request is made of, category by category, with an honest token estimate.
//!
//! The accounting is computed from the exact `messages` + `tools` the runtime
//! is about to send — never from a UI transcript and never from a second,
//! parallel prompt assembly. The token figures are estimates unless a real
//! tokenizer exists (none of the current providers exposes one), so a snapshot
//! is marked [`TokenCountKind::Estimated`] and the UI must never dress it up
//! as exact.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use leveler_core::ToolCallId;

use crate::message::{ContentPart, Role};
use crate::projection::{ReasoningProjectionSummary, RequestProjection};
use crate::request::ModelRef;

/// Stable prefix of the fold breadcrumb the runtime injects as a user message
/// when it compacts history. Defined here because the breadcrumb is
/// model-visible content: `leveler-context` builds it and context accounting
/// detects it, so both read one constant instead of two that can drift.
pub const COMPACTION_BREADCRUMB_MARKER: &str = "[Earlier context was compacted";

/// How the token figures in a snapshot were produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TokenCountKind {
    /// Every figure came from a real tokenizer.
    Exact,
    /// Every figure came from the byte/char estimator.
    Estimated,
    /// The total is provider-reported but the breakdown is estimated.
    Mixed,
}

/// What a measured request's pressure means for the context fold, and
/// therefore what a failed briefing costs the turn.
///
/// The fold has TWO bounds, and they answer different questions. The quality
/// threshold is where recall is expected to degrade — folding there is a
/// quality choice that may be abandoned. The hard capacity is where a request
/// can no longer legally be sent — folding there is REQUIRED, and a fold that
/// cannot be produced must still be brought under capacity mechanically or the
/// turn fails explicitly.
///
/// This is the ONE contract every active-context entry shares: coding/drive,
/// chat, resume and the accounting that REPORTS which state a request is in.
/// Entries provide the measured facts (projected tokens, resolved bounds); this
/// decides what a failure means — and, because the display reads this same
/// function, a shown trigger state can never disagree with the decision.
///
/// It lives beside the threshold arithmetic ([`ModelLimits`]) rather than in a
/// harness: the classification is a comparison of three numbers and every
/// consumer must read the same implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FoldRequirement {
    /// At or below the quality boundary: nothing to fold.
    None,
    /// Over the quality boundary but within hard capacity. Sending the request
    /// uncompacted is legal, so a failed briefing keeps the original active
    /// history and the turn continues.
    Soft,
    /// Over the hard capacity. Without a fold the request cannot legally be
    /// sent, so a failed briefing must be folded mechanically or the turn
    /// fails without ever sending an oversized request.
    HardRequired,
}

impl FoldRequirement {
    /// Classify from facts only.
    ///
    /// `hard_capacity` is `None` when the model declares no window: with no
    /// hard limit there is no request compaction is obliged to make legal, so
    /// nothing may claim a hard requirement.
    pub fn classify(
        projected_tokens: u64,
        quality_threshold: u64,
        hard_capacity: Option<u64>,
    ) -> Self {
        if let Some(capacity) = hard_capacity
            && projected_tokens > capacity
        {
            return Self::HardRequired;
        }
        if projected_tokens > quality_threshold {
            return Self::Soft;
        }
        Self::None
    }

    /// Whether a fold must succeed (mechanically at minimum) before the next
    /// request can be sent.
    pub fn fold_is_mandatory(self) -> bool {
        matches!(self, Self::HardRequired)
    }
}

/// Deterministic context-pressure level derived from real thresholds — never
/// a model's judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ContextPressure {
    Normal,
    Warning,
    Critical,
}

/// One mutually exclusive slice of the request. `children` holds the next
/// level of drill-down when the runtime can reliably separate it; a category
/// with no reliable sub-split has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContextCategory {
    /// Stable key (`messages`, `system`, `tool_definitions`, `user`, … or a
    /// tool name for a per-tool leaf). The UI may localize by `name`; it must
    /// never parse `label`.
    pub name: String,
    /// Presentation label (English, not localized here).
    pub label: String,
    /// Estimated tokens in this slice. Summing the top level equals
    /// [`ContextAccounting::used_tokens`] by construction.
    pub tokens: u64,
    /// Tool invocations counted in this slice (nonzero only for tool leaves
    /// and `tool_calls`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub calls: u64,
    /// Next drill-down level, when one exists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ContextCategory>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// One recorded compaction fold: the estimated transcript size before and
/// after. The runtime records this at the fold boundary, not the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CompactionRecord {
    pub before_tokens: u64,
    pub after_tokens: u64,
}

impl CompactionRecord {
    pub fn reclaimed_tokens(&self) -> u64 {
        self.before_tokens.saturating_sub(self.after_tokens)
    }

    /// Share reclaimed in `0.0..=1.0` (0 when `before` was 0).
    pub fn reclaimed_ratio(&self) -> f64 {
        if self.before_tokens == 0 {
            return 0.0;
        }
        self.reclaimed_tokens() as f64 / self.before_tokens as f64
    }
}

/// What one assembled model request is made of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ContextAccounting {
    pub model: ModelRef,
    /// The model's declared context window (an exact declared fact), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,
    /// The SOFT fold threshold: where compaction is expected and may be
    /// abandoned. It is strictly below [`Self::input_capacity_tokens`] whenever
    /// both are known, because a percentage policy reserves the completion
    /// first and takes its share of what is left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_at_tokens: Option<u64>,
    /// The completion this harness reserves per request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_reservation_tokens: Option<u64>,
    /// Extra safety margin on top of the completion reservation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headroom_tokens: Option<u64>,
    /// The EFFECTIVE INPUT CAPACITY: `window − reservation − headroom`, when a
    /// window is declared and leaves input room. This — never the whole model
    /// window — is the denominator of compaction utilization, and it is the same
    /// number [`Self::fold_state`] compares against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_capacity_tokens: Option<u64>,
    /// The exact decision for THIS request, from the one classifier the harness
    /// folds with. A display may not re-derive it from a rounded percentage.
    pub fold_state: FoldRequirement,
    /// Estimated input tokens of this request — the sum of the top-level
    /// categories, by construction.
    pub used_tokens: u64,
    /// `context_window - used` when the window is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_tokens: Option<u64>,
    pub token_count_kind: TokenCountKind,
    pub pressure: ContextPressure,
    pub categories: Vec<ContextCategory>,
    /// Most recent compaction fold, when one has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction: Option<CompactionRecord>,
    /// What the request projection did with the requested reasoning-retention
    /// arm: which turns carried reasoning, and how many the route contract had
    /// to keep against the request. Present on every snapshot computed from a
    /// projection, so an overridden treatment is visible rather than assumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_projection: Option<ReasoningProjectionSummary>,
}

/// Per-slice estimator state: the SAME accumulator the compaction-pressure
/// estimator uses ([`crate::TokenEstimate`]), so a slice total and a
/// whole-transcript total are one arithmetic on one set of weights. Two
/// independent estimators were how the accounting counted reasoning the
/// pressure estimate could not see.
type SliceTokens = crate::TokenEstimate;

/// A slice being accumulated: its stable name, label, estimator state, and the
/// tool call count it carries (for tool leaves).
#[derive(Debug)]
struct Slice {
    name: String,
    label: String,
    tokens: SliceTokens,
    calls: u64,
}

impl Slice {
    fn new(name: &str, label: &str) -> Self {
        Self {
            name: name.to_string(),
            label: label.to_string(),
            tokens: SliceTokens::default(),
            calls: 0,
        }
    }

    fn finish(&self) -> ContextCategory {
        ContextCategory {
            name: self.name.clone(),
            label: self.label.clone(),
            tokens: self.tokens.tokens(),
            calls: self.calls,
            children: Vec::new(),
        }
    }
}

impl ContextAccounting {
    /// Project one assembled request into its mutually exclusive slices.
    ///
    /// `projection` is the provider-visible view of the request — the SAME
    /// decision the wire encoder serializes. The accounting therefore cannot
    /// count a block the provider will not see (the drift that made historical
    /// reasoning look present for routes that never received it), and it needs
    /// to understand nothing about JSON, HTTP or wire structs to do it.
    /// `context_window` is the model's declared window (exact fact);
    /// `compact_at` is the pressure threshold; `last_compaction` is the most
    /// recent fold recorded by the harness.
    pub fn compute(
        model: ModelRef,
        projection: &RequestProjection,
        context_window: Option<u32>,
        compact_at: Option<u32>,
        last_compaction: Option<CompactionRecord>,
    ) -> Self {
        let messages = projection.messages();
        let tools = projection.tools();
        // Build the call_id → tool-name map from assistant tool calls, so a
        // tool result can be attributed to the tool that produced it without
        // guessing from its content.
        let mut call_names: HashMap<ToolCallId, String> = HashMap::new();
        for message in messages {
            for part in &message.content {
                if let ContentPart::ToolCall { call } = part {
                    call_names.insert(call.id.clone(), call.name.clone());
                }
            }
        }

        let mut system = Slice::new("system", "System");
        system.tokens.add_text(&projection.control_text());
        let mut user = Slice::new("user", "User");
        let mut assistant = Slice::new("assistant", "Assistant");
        // Historical reasoning is the one slice whose size someone will ask
        // about by name ("why 80k input?"). It stays its own category instead
        // of hiding inside `assistant`, and it is counted — an estimator that
        // skipped it could not answer that question at all.
        let mut reasoning = Slice::new("reasoning", "Reasoning");
        let mut tool_calls = Slice::new("tool_calls", "Tool calls");
        let mut compaction = Slice::new("compaction_summary", "Compaction summary");
        let mut other = Slice::new("other", "Other");
        // Per-tool result leaves, keyed by tool name, preserving first-seen
        // order for a deterministic tie-break.
        let mut tool_result_order: Vec<String> = Vec::new();
        let mut tool_results: HashMap<String, Slice> = HashMap::new();

        for message in messages {
            // The reasoning channel is a projection decision, not content:
            // it is counted exactly when (and only when) the provider carries it.
            if let Some(text) = message.reasoning.as_wire() {
                reasoning.tokens.add_text(text);
            }
            for part in &message.content {
                match part {
                    ContentPart::Text { text } => match message.role {
                        Role::System => system.tokens.add_text(text),
                        Role::User if text.contains(COMPACTION_BREADCRUMB_MARKER) => {
                            compaction.tokens.add_text(text)
                        }
                        Role::User => user.tokens.add_text(text),
                        Role::Assistant => assistant.tokens.add_text(text),
                        Role::Tool => other.tokens.add_text(text),
                    },
                    ContentPart::SignedReasoning { .. } | ContentPart::RedactedReasoning { .. } => {
                        reasoning.tokens.add_part(part)
                    }
                    ContentPart::ToolCall { call } => {
                        tool_calls.tokens.add_tool(&call.name);
                        tool_calls.tokens.add_tool(&call.arguments.to_string());
                        tool_calls.calls += 1;
                    }
                    ContentPart::ToolResult { result } => {
                        let name = call_names
                            .get(&result.call_id)
                            .cloned()
                            .unwrap_or_else(|| "other".to_string());
                        if !tool_results.contains_key(&name) {
                            tool_result_order.push(name.clone());
                            tool_results.insert(name.clone(), Slice::new(&name, &name));
                        }
                        let leaf = tool_results.get_mut(&name).expect("just inserted");
                        leaf.tokens.add_tool(&result.content);
                        leaf.calls += 1;
                    }
                    ContentPart::Image { .. } => other.tokens.add_image(),
                    // A projected message never carries a reasoning part: the
                    // channel above is the only representation. Counting it
                    // here as well would double-count it.
                    ContentPart::Reasoning { .. } => {}
                }
            }
        }

        let mut tool_definitions = Slice::new("tool_definitions", "Tool definitions");
        for tool in tools {
            tool_definitions.tokens.add_tool_definition(tool);
        }

        // Drill-down: Messages → user / assistant / tool calls / tool results
        // / compaction summary; Tool results → per-tool leaves.
        let mut tool_result_children: Vec<ContextCategory> = tool_result_order
            .iter()
            .map(|name| tool_results[name].finish())
            .collect();
        tool_result_children.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.name.cmp(&b.name)));
        // The parent total is the exact sum of its (already divided) leaves,
        // so a tool result is never double counted and the parent always
        // equals the sum of its children.
        let tool_results_tokens: u64 = tool_result_children.iter().map(|c| c.tokens).sum();
        let tool_results_calls: u64 = tool_result_children.iter().map(|c| c.calls).sum();
        let tool_results_cat = ContextCategory {
            name: "tool_results".to_string(),
            label: "Tool results".to_string(),
            tokens: tool_results_tokens,
            calls: tool_results_calls,
            children: tool_result_children,
        };

        // Messages children.
        let mut messages_children = vec![
            user.finish(),
            assistant.finish(),
            reasoning.finish(),
            tool_calls.finish(),
            tool_results_cat,
            compaction.finish(),
        ];
        // Keep zero-token children out of the display but preserve the parent
        // total as their exact sum.
        let messages_tokens: u64 = messages_children.iter().map(|c| c.tokens).sum();
        messages_children.retain(|c| c.tokens > 0);

        let messages = Slice::new("messages", "Messages");
        let mut messages_cat = messages.finish();
        messages_cat.tokens = messages_tokens;
        messages_cat.children = messages_children;

        let mut top: Vec<ContextCategory> = vec![
            messages_cat,
            system.finish(),
            tool_definitions.finish(),
            other.finish(),
        ];
        top.retain(|c| c.tokens > 0);
        top.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.name.cmp(&b.name)));

        let used_tokens: u64 = top.iter().map(|c| c.tokens).sum();

        let free_tokens = context_window.map(|w| u64::from(w).saturating_sub(used_tokens));

        Self {
            model,
            context_window_tokens: context_window.map(u64::from),
            compact_at_tokens: compact_at.map(u64::from),
            // A caller that has not supplied the resolved budget has not
            // claimed a hard bound; `with_input_budget` adds it from the SAME
            // policy the request was folded with.
            output_reservation_tokens: None,
            headroom_tokens: None,
            input_capacity_tokens: None,
            fold_state: FoldRequirement::classify(
                used_tokens,
                compact_at.map(u64::from).unwrap_or(u64::MAX),
                None,
            ),
            used_tokens,
            free_tokens,
            token_count_kind: TokenCountKind::Estimated,
            pressure: compute_pressure(
                used_tokens,
                context_window.map(u64::from),
                compact_at.map(u64::from),
                None,
            ),
            categories: top,
            last_compaction,
            reasoning_projection: Some(projection.summary().clone()),
        }
    }

    /// Attach the resolved budget this request was measured against, from the
    /// harness's [`ResolvedContextPolicy`] — not re-derived here.
    ///
    /// The thresholds are the policy's own numbers; this only publishes them
    /// beside the request they judged, and re-derives the state with the same
    /// classifier so a display can never disagree with the fold decision.
    pub fn with_input_budget(
        mut self,
        input_capacity: Option<u32>,
        output_reservation: Option<u32>,
        headroom: Option<u32>,
    ) -> Self {
        self.input_capacity_tokens = input_capacity.map(u64::from);
        self.output_reservation_tokens = output_reservation.map(u64::from);
        self.headroom_tokens = headroom.map(u64::from);
        self.fold_state = FoldRequirement::classify(
            self.used_tokens,
            self.compact_at_tokens.unwrap_or(u64::MAX),
            self.input_capacity_tokens,
        );
        self.pressure = compute_pressure(
            self.used_tokens,
            self.context_window_tokens,
            self.compact_at_tokens,
            self.input_capacity_tokens,
        );
        self
    }
}

impl ContextAccounting {
    /// Compaction utilization in whole percent: projected input over the
    /// EFFECTIVE INPUT CAPACITY. `None` when no capacity is known — a caller
    /// must not substitute the model window and call it utilization.
    pub fn compaction_utilization_percent(&self) -> Option<u64> {
        Self::percent(self.used_tokens, self.input_capacity_tokens?)
    }

    /// Model-window utilization in whole percent: projected input over the
    /// model's declared window. A DIFFERENT axis from
    /// [`Self::compaction_utilization_percent`] and never a substitute for it.
    pub fn window_utilization_percent(&self) -> Option<u64> {
        Self::percent(self.used_tokens, self.context_window_tokens?)
    }

    /// Whole percent of `part` in `whole`, saturating and never dividing by
    /// zero. The one place the client-facing ratios are computed.
    fn percent(part: u64, whole: u64) -> Option<u64> {
        (whole > 0).then(|| part.saturating_mul(100) / whole)
    }

    /// Estimated tokens of the historical-reasoning channel in THIS request —
    /// exactly the projection the wire carries, and zero when the route
    /// carries none.
    ///
    /// It is the `reasoning` category of the breakdown, exposed as a number so
    /// "of this round's input, how much was replayed thinking?" is answerable
    /// without walking categories. The number comes from the same
    /// [`RequestProjection`] the wire encoder serializes, so it can never price
    /// reasoning a provider does not see.
    pub fn projected_reasoning_tokens(&self) -> u64 {
        self.categories
            .iter()
            .find(|category| category.name == "messages")
            .and_then(|messages| {
                messages
                    .children
                    .iter()
                    .find(|child| child.name == "reasoning")
            })
            .map(|reasoning| reasoning.tokens)
            .unwrap_or(0)
    }
}

/// `used` against the resolved thresholds. Deterministic, and derived from the
/// SAME classifier the fold uses ([`FoldRequirement::classify`]) so the shown
/// state is the decision, not an approximation of it.
///
/// `Warning` is presentation only: approaching the soft threshold, measured
/// against the real number (80% of it), never against a rounded percentage.
fn compute_pressure(
    used: u64,
    context_window: Option<u64>,
    compact_at: Option<u64>,
    input_capacity: Option<u64>,
) -> ContextPressure {
    if FoldRequirement::classify(used, compact_at.unwrap_or(u64::MAX), input_capacity)
        != FoldRequirement::None
    {
        return ContextPressure::Critical;
    }
    // No hard bound could be formed, but the physical window is known and the
    // request has reached it: still Critical, and never a silent "Normal".
    if input_capacity.is_none()
        && let Some(window) = context_window
        && window > 0
        && used >= window
    {
        return ContextPressure::Critical;
    }
    if let Some(threshold) = compact_at
        && threshold > 0
        && used >= threshold * 8 / 10
    {
        return ContextPressure::Warning;
    }
    ContextPressure::Normal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, ToolDefinition};
    use crate::projection::{
        MissingReasoningReplay, ReasoningReplayContract, ReasoningReplayScope,
    };
    use crate::retention::ReasoningRetention;
    use leveler_core::ToolCallId;

    /// A route that carries captured reasoning on every request it spans, so an
    /// accounting test measures the channel rather than a route's silence.
    fn reasoning_route() -> ReasoningReplayContract {
        ReasoningReplayContract::raw_field(
            ReasoningReplayScope::Always,
            MissingReasoningReplay::Omit,
        )
    }

    /// The provider-visible view of `messages` under a reasoning-carrying route.
    fn projection(messages: &[Message], tools: &[ToolDefinition]) -> RequestProjection {
        RequestProjection::project(messages, tools, reasoning_route(), ReasoningRetention::All)
    }

    fn model() -> ModelRef {
        ModelRef::new("deepseek", "deepseek-chat")
    }

    #[test]
    fn control_context_is_counted_without_polluting_transcript() {
        let messages = vec![Message::text(Role::User, "work")];
        let control = crate::ControlContext {
            blocks: vec![
                crate::PromptSegment::stable("base", "base instructions"),
                crate::PromptSegment::variable("rules", "current project rules"),
            ],
        };
        let projected = RequestProjection::project_with_control_context(
            &messages,
            &[],
            reasoning_route(),
            ReasoningRetention::All,
            &control,
        );
        let plain = projection(&messages, &[]);
        assert_eq!(projected.messages(), plain.messages());
        assert_eq!(projected.control_context(), &control);
        assert_eq!(
            projected.control_text(),
            "base instructions\n\ncurrent project rules"
        );
        let accounting = ContextAccounting::compute(model(), &projected, None, None, None);
        assert_eq!(accounting.used_tokens, projected.estimated_tokens());
        assert!(projected.estimated_tokens() > plain.estimated_tokens());
        let restored: RequestProjection =
            serde_json::from_value(serde_json::to_value(&projected).unwrap()).unwrap();
        assert_eq!(restored.control_context(), &control);
    }

    fn tool_call(name: &str, id: &str, args: serde_json::Value) -> ContentPart {
        ContentPart::ToolCall {
            call: crate::message::ToolCall {
                id: ToolCallId::new(id),
                name: name.to_string(),
                arguments: args,
            },
        }
    }

    fn tool_result(id: &str, content: &str) -> ContentPart {
        ContentPart::ToolResult {
            result: crate::message::ToolResultContent {
                call_id: ToolCallId::new(id),
                content: content.to_string(),
                is_error: false,
            },
        }
    }

    fn text(role: Role, s: &str) -> Message {
        Message::text(role, s)
    }

    fn category<'a>(acc: &'a ContextAccounting, name: &str) -> &'a ContextCategory {
        acc.categories
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("missing category {name}: {:?}", acc.categories))
    }

    #[test]
    fn empty_session_has_zero_used() {
        let acc = ContextAccounting::compute(
            model(),
            &projection(&[], &[]),
            Some(128_000),
            Some(64_000),
            None,
        );
        assert_eq!(acc.used_tokens, 0);
        assert!(acc.categories.is_empty());
        assert_eq!(acc.free_tokens, Some(128_000));
        assert_eq!(acc.pressure, ContextPressure::Normal);
        assert_eq!(acc.token_count_kind, TokenCountKind::Estimated);
    }

    #[test]
    fn system_only_session_counts_system() {
        let messages = vec![text(Role::System, "you are an agent")];
        let acc = ContextAccounting::compute(
            model(),
            &projection(&messages, &[]),
            Some(128_000),
            Some(64_000),
            None,
        );
        let system = category(&acc, "system");
        assert!(system.tokens > 0);
        assert_eq!(acc.used_tokens, system.tokens);
    }

    #[test]
    fn category_sum_equals_used() {
        let messages = vec![
            text(Role::System, "you are an agent"),
            text(Role::User, "fix the bug"),
            Message {
                origin: None,
                role: Role::Assistant,
                content: vec![
                    text(Role::Assistant, "reading").content[0].clone(),
                    tool_call("read_file", "c1", serde_json::json!({"path": "a"})),
                ],
            },
            Message {
                origin: None,
                role: Role::Tool,
                content: vec![tool_result("c1", &"x".repeat(4000))],
            },
        ];
        let tools = vec![ToolDefinition {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let acc = ContextAccounting::compute(
            model(),
            &projection(&messages, &tools),
            Some(128_000),
            Some(64_000),
            None,
        );
        let sum: u64 = acc.categories.iter().map(|c| c.tokens).sum();
        assert_eq!(acc.used_tokens, sum);
        // The drill-down is a breakdown, never a second copy: Messages equals
        // the sum of its children, so nothing is counted twice.
        let messages = category(&acc, "messages");
        let children: u64 = messages.children.iter().map(|c| c.tokens).sum();
        assert_eq!(messages.tokens, children);
        let results = messages
            .children
            .iter()
            .find(|c| c.name == "tool_results")
            .expect("tool_results child");
        let leaves: u64 = results.children.iter().map(|c| c.tokens).sum();
        assert_eq!(results.tokens, leaves);
    }

    /// The accounting and the pressure estimator are ONE arithmetic: the
    /// snapshot's total is [`crate::estimate_tokens`] over the same messages
    /// plus the same tool schemas — same buckets, same weights, same
    /// accumulator type (`SliceTokens` IS the shared `TokenEstimate`). The two
    /// differ by at most one token per priced slice, and only because the
    /// accounting divides each slice once (so the drill-down sums to the
    /// total) where the whole-transcript estimate divides once. A projection
    /// change cannot move one number without the other.
    #[test]
    fn accounting_total_is_the_shared_estimator_over_the_same_request() {
        let messages = vec![
            text(Role::System, "rules"),
            text(Role::User, "inspect"),
            Message {
                origin: None,
                role: Role::Assistant,
                content: vec![
                    ContentPart::Reasoning {
                        text: "thinking about it".repeat(50),
                    },
                    tool_call("read_file", "c1", serde_json::json!({"path": "a.rs"})),
                ],
            },
            Message {
                origin: None,
                role: Role::Tool,
                content: vec![tool_result("c1", &"body ".repeat(200))],
            },
            Message {
                origin: None,
                role: Role::User,
                content: vec![ContentPart::Image {
                    source: crate::message::ImageSource::Url {
                        url: "https://x/y.png".into(),
                    },
                }],
            },
        ];
        let tools = vec![ToolDefinition {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let acc =
            ContextAccounting::compute(model(), &projection(&messages, &tools), None, None, None);
        let shared = crate::estimate_tokens(&messages) + crate::estimate_tool_definitions(&tools);
        // One token per priced slice is the most integer-division placement
        // can differ by; anything larger is a different formula or different
        // inputs, which is the drift this invariant exists to catch.
        assert!(
            acc.used_tokens.abs_diff(shared) <= acc.categories.len() as u64,
            "accounting and compaction pressure must price the same request: \
             accounting={} shared_estimator={shared}",
            acc.used_tokens
        );
    }

    /// Reasoning is its own category, and dropping it from the request must
    /// move the total by exactly the estimator's price for it.
    #[test]
    fn reasoning_has_its_own_category_and_is_counted() {
        let reasoning_text = "why ".repeat(400);
        let with = vec![Message {
            origin: None,
            role: Role::Assistant,
            content: vec![
                ContentPart::Reasoning {
                    text: reasoning_text.clone(),
                },
                ContentPart::Text {
                    text: "done".into(),
                },
            ],
        }];
        let without = vec![Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::Text {
                text: "done".into(),
            }],
        }];
        let acc = ContextAccounting::compute(model(), &projection(&with, &[]), None, None, None);
        let reasoning = category(&acc, "messages")
            .children
            .iter()
            .find(|c| c.name == "reasoning")
            .expect("reasoning child");
        assert_eq!(
            reasoning.tokens,
            crate::estimate_tokens(&with) - crate::estimate_tokens(&without)
        );
        let without_acc =
            ContextAccounting::compute(model(), &projection(&without, &[]), None, None, None);
        assert_eq!(acc.used_tokens - without_acc.used_tokens, reasoning.tokens);
        // The same number is exposed directly, and it is zero on a route that
        // carries no reasoning channel: the figure is the projection's, not a
        // recount of the semantic transcript.
        assert_eq!(acc.projected_reasoning_tokens(), reasoning.tokens);
        assert_eq!(without_acc.projected_reasoning_tokens(), 0);
        let silent_route = RequestProjection::project(
            &with,
            &[],
            ReasoningReplayContract::NONE,
            ReasoningRetention::All,
        );
        let silent = ContextAccounting::compute(model(), &silent_route, None, None, None);
        assert_eq!(silent.projected_reasoning_tokens(), 0);
        assert_eq!(silent.used_tokens, without_acc.used_tokens);
    }

    #[test]
    fn large_tool_result_is_visible_in_context_breakdown() {
        let small = "hello";
        let large = "y".repeat(40_000);
        let messages = vec![
            text(Role::System, "you are an agent"),
            text(Role::User, "inspect"),
            Message {
                origin: None,
                role: Role::Assistant,
                content: vec![tool_call(
                    "run_command",
                    "c1",
                    serde_json::json!({"cmd": "test"}),
                )],
            },
            Message {
                origin: None,
                role: Role::Tool,
                content: vec![tool_result("c1", &large)],
            },
            Message {
                origin: None,
                role: Role::Assistant,
                content: vec![text(Role::Assistant, small).content[0].clone()],
            },
        ];
        let acc = ContextAccounting::compute(
            model(),
            &projection(&messages, &[]),
            Some(128_000),
            Some(64_000),
            None,
        );
        let messages_cat = category(&acc, "messages");
        let tool_results = messages_cat
            .children
            .iter()
            .find(|c| c.name == "tool_results")
            .expect("tool_results child");
        let assistant = messages_cat
            .children
            .iter()
            .find(|c| c.name == "assistant")
            .expect("assistant child");
        assert!(
            tool_results.tokens > assistant.tokens * 5,
            "a huge tool result must dwarf the assistant text: tool_results={} assistant={}",
            tool_results.tokens,
            assistant.tokens
        );
        // The tool result is attributed to the tool that produced it.
        let leaf = tool_results
            .children
            .iter()
            .find(|c| c.name == "run_command")
            .expect("run_command leaf");
        assert_eq!(leaf.calls, 1);
        assert!(leaf.tokens > 0);
    }

    #[test]
    fn tool_result_with_unknown_call_id_falls_into_other() {
        let messages = vec![Message {
            origin: None,
            role: Role::Tool,
            content: vec![tool_result("never-seen", "body")],
        }];
        let acc =
            ContextAccounting::compute(model(), &projection(&messages, &[]), None, None, None);
        let messages_cat = category(&acc, "messages");
        let tool_results = messages_cat
            .children
            .iter()
            .find(|c| c.name == "tool_results")
            .unwrap();
        assert!(
            tool_results.children.iter().any(|c| c.name == "other"),
            "unattributable result should land in `other`: {:?}",
            tool_results.children
        );
    }

    #[test]
    fn compaction_breadcrumb_is_not_double_counted_as_user() {
        let messages = vec![
            text(Role::System, "sys"),
            text(Role::User, "task"),
            text(
                Role::User,
                &format!("{COMPACTION_BREADCRUMB_MARKER} to fit the window: 9 steps elided.]"),
            ),
        ];
        let acc =
            ContextAccounting::compute(model(), &projection(&messages, &[]), None, None, None);
        let messages_cat = category(&acc, "messages");
        let compaction = messages_cat
            .children
            .iter()
            .find(|c| c.name == "compaction_summary")
            .expect("compaction summary child");
        let user = messages_cat
            .children
            .iter()
            .find(|c| c.name == "user")
            .expect("user child");
        assert!(compaction.tokens > 0);
        // "task" is user; the breadcrumb is compaction — no overlap.
        assert_eq!(user.tokens, 1);
    }

    #[test]
    fn tool_definitions_are_counted_separately_from_messages() {
        let tools = vec![ToolDefinition {
            name: "read_file".into(),
            description: "read a file at a path".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let acc = ContextAccounting::compute(model(), &projection(&[], &tools), None, None, None);
        let defs = category(&acc, "tool_definitions");
        assert!(defs.tokens > 0);
        assert_eq!(acc.used_tokens, defs.tokens);
    }

    #[test]
    fn pressure_tracks_the_fold_threshold_deterministically() {
        let win = Some(128_000u32);
        let fold = Some(64_000u32);
        let capacity = Some(96_000u64);
        let p =
            |used: u64| compute_pressure(used, win.map(u64::from), fold.map(u64::from), capacity);
        // Below 80% of the soft threshold → normal.
        assert_eq!(p(40_000), ContextPressure::Normal);
        // 80%..soft → warning; exactly AT the soft threshold is still not a
        // fold (`FoldRequirement` is strictly "over"), so it stays a warning.
        assert_eq!(p(55_000), ContextPressure::Warning);
        assert_eq!(p(64_000), ContextPressure::Warning);
        // ONE token over the soft threshold is the real Soft state → critical.
        assert_eq!(p(64_001), ContextPressure::Critical);
        // Over the hard capacity is the real Hard state → critical.
        assert_eq!(p(96_001), ContextPressure::Critical);
        // At the hard capacity the request is still legal, so the classifier
        // says Soft, and the display follows the classifier.
        assert_eq!(p(96_000), ContextPressure::Critical);
        // No hard bound could be formed, but the physical window is reached:
        // critical, never a silent normal.
        assert_eq!(
            compute_pressure(128_000, win.map(u64::from), None, None),
            ContextPressure::Critical
        );
    }

    /// The pressure level is the real fold decision plus a presentation band —
    /// it is never re-derived from a rounded percentage.
    #[test]
    fn pressure_follows_the_classifier_at_every_boundary() {
        for (used, soft, capacity) in [
            (0u64, 100u64, Some(200u64)),
            (79, 100, Some(200)),
            (80, 100, Some(200)),
            (99, 100, Some(200)),
            (100, 100, Some(200)),
            (101, 100, Some(200)),
            (200, 100, Some(200)),
            (201, 100, Some(200)),
        ] {
            let state = FoldRequirement::classify(used, soft, capacity);
            let pressure = compute_pressure(used, None, Some(soft), capacity);
            match state {
                FoldRequirement::None => assert_ne!(pressure, ContextPressure::Critical, "{used}"),
                _ => assert_eq!(pressure, ContextPressure::Critical, "{used}"),
            }
        }
    }

    #[test]
    fn free_tokens_is_window_minus_used_and_never_underflows() {
        let messages = vec![text(Role::User, &"x".repeat(1_000_000))];
        let acc = ContextAccounting::compute(
            model(),
            &projection(&messages, &[]),
            Some(128_000),
            None,
            None,
        );
        assert_eq!(
            acc.free_tokens,
            Some(0),
            "free must clamp at zero, not underflow"
        );
        assert_eq!(acc.pressure, ContextPressure::Critical);
    }

    #[test]
    fn unknown_window_reports_no_free_and_no_fake_ratio() {
        let messages = vec![text(Role::User, "hi")];
        let acc =
            ContextAccounting::compute(model(), &projection(&messages, &[]), None, None, None);
        assert_eq!(acc.context_window_tokens, None);
        assert_eq!(acc.free_tokens, None);
        assert_eq!(
            acc.compaction_utilization_percent(),
            None,
            "a utilization number needs a real input capacity, not a window"
        );
    }

    /// The two utilization axes are separate, and compaction utilization is
    /// over the EFFECTIVE INPUT CAPACITY — never the whole model window.
    #[test]
    fn utilization_axes_use_their_own_denominators() {
        let messages = vec![text(Role::User, &"x".repeat(4_000))];
        let acc = ContextAccounting::compute(
            model(),
            &projection(&messages, &[]),
            Some(128_000),
            Some(64_000),
            None,
        )
        .with_input_budget(Some(96_000), Some(32_000), Some(0));
        assert_eq!(acc.used_tokens, 1_000);
        assert_eq!(acc.input_capacity_tokens, Some(96_000));
        assert_eq!(acc.output_reservation_tokens, Some(32_000));
        assert_eq!(acc.headroom_tokens, Some(0));
        assert_eq!(acc.compaction_utilization_percent(), Some(1));
        assert_eq!(acc.window_utilization_percent(), Some(0));
        assert_eq!(acc.fold_state, FoldRequirement::None);
        assert_eq!(acc.pressure, ContextPressure::Normal);
    }

    #[test]
    fn cjk_text_is_weighted_by_character_not_four_bytes() {
        let cjk = vec![text(Role::User, &"修".repeat(1000))];
        let ascii = vec![text(Role::User, &"a".repeat(3000))];
        let cjk_acc = ContextAccounting::compute(model(), &projection(&cjk, &[]), None, None, None);
        let ascii_acc =
            ContextAccounting::compute(model(), &projection(&ascii, &[]), None, None, None);
        assert!(
            cjk_acc.used_tokens >= 950,
            "CJK must not be under-counted: {}",
            cjk_acc.used_tokens
        );
        // 1000 CJK chars (~1000 tokens) vs 3000 ascii (~750 tokens).
        assert!(cjk_acc.used_tokens > ascii_acc.used_tokens);
    }

    #[test]
    fn marker_matches_what_compaction_writes() {
        // The accounting detects the breadcrumb by this exact prefix. If the
        // compaction writer changes its wording, this lock fails first.
        assert_eq!(
            COMPACTION_BREADCRUMB_MARKER,
            "[Earlier context was compacted"
        );
    }
}
