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

use crate::message::{ContentPart, Message, Role, ToolDefinition};
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
    /// The fold threshold (`reliable_context`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_at_tokens: Option<u64>,
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
}

/// A conservative flat cost (in ASCII byte-equivalents) for one image, so a
/// vision turn is not counted as ~free. Mirrors `leveler-context`'s
/// `IMAGE_BYTE_EQUIV` so the two estimates cannot drift.
const IMAGE_BYTE_EQUIV: u64 = 4096;

/// (ascii bytes, wide bytes) split. Non-ASCII (CJK, …) spends ~1 token per
/// character (~3 UTF-8 bytes), so those bytes are weighted at 3 bytes/token
/// rather than the flat ÷4 used for ASCII prose.
fn split(s: &str) -> (u64, u64) {
    let ascii = s.bytes().filter(u8::is_ascii).count() as u64;
    (ascii, s.len() as u64 - ascii)
}

/// Per-slice estimator state. Accumulates the same four buckets the
/// `leveler-context` calibrated estimator uses, then divides once, so the
/// weights stay identical.
#[derive(Debug, Default)]
struct SliceTokens {
    ascii_text: u64,
    ascii_tool: u64,
    wide: u64,
    flat: u64,
}

impl SliceTokens {
    fn add_text(&mut self, s: &str) {
        let (a, w) = split(s);
        self.ascii_text += a;
        self.wide += w;
    }

    /// Tool payloads are JSON/log shaped and tokenize far denser than prose:
    /// ~2.5–2.9 bytes/token measured against DeepSeek-reported usage, where a
    /// flat ÷4 under-counted by 27–38%. Weighted at 2.5 so the residual error
    /// sits on the safe side.
    fn add_tool(&mut self, s: &str) {
        let (a, w) = split(s);
        self.ascii_tool += a;
        self.wide += w;
    }

    fn add_image(&mut self) {
        self.flat += IMAGE_BYTE_EQUIV / 4;
    }

    fn total(&self) -> u64 {
        self.ascii_text / 4 + self.ascii_tool * 2 / 5 + self.wide / 3 + self.flat
    }
}

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
            tokens: self.tokens.total(),
            calls: self.calls,
            children: Vec::new(),
        }
    }
}

impl ContextAccounting {
    /// Project one assembled request into its mutually exclusive slices.
    ///
    /// `messages` and `tools` are the exact payload the runtime is about to
    /// send; `context_window` is the model's declared window (exact fact);
    /// `compact_at` is the fold threshold; `last_compaction` is the most
    /// recent fold recorded by the harness.
    pub fn compute(
        model: ModelRef,
        messages: &[Message],
        tools: &[ToolDefinition],
        context_window: Option<u32>,
        compact_at: Option<u32>,
        last_compaction: Option<CompactionRecord>,
    ) -> Self {
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
        let mut user = Slice::new("user", "User");
        let mut assistant = Slice::new("assistant", "Assistant");
        let mut tool_calls = Slice::new("tool_calls", "Tool calls");
        let mut compaction = Slice::new("compaction_summary", "Compaction summary");
        let mut other = Slice::new("other", "Other");
        // Per-tool result leaves, keyed by tool name, preserving first-seen
        // order for a deterministic tie-break.
        let mut tool_result_order: Vec<String> = Vec::new();
        let mut tool_results: HashMap<String, Slice> = HashMap::new();

        for message in messages {
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
                    ContentPart::Reasoning { text } => {
                        // Reasoning is assistant content and is passed back to
                        // providers that require it; count it as assistant.
                        assistant.tokens.add_text(text);
                    }
                    ContentPart::ToolCall { call } => {
                        let mut s = call.name.clone();
                        s.push_str(&call.arguments.to_string());
                        tool_calls.tokens.add_tool(&s);
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
                }
            }
        }

        let mut tool_definitions = Slice::new("tool_definitions", "Tool definitions");
        for tool in tools {
            tool_definitions.tokens.add_text(&tool.description);
            tool_definitions.tokens.add_tool(&tool.name);
            tool_definitions
                .tokens
                .add_tool(&tool.input_schema.to_string());
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
        let pressure = compute_pressure(
            used_tokens,
            context_window.map(u64::from),
            compact_at.map(u64::from),
        );

        Self {
            model,
            context_window_tokens: context_window.map(u64::from),
            compact_at_tokens: compact_at.map(u64::from),
            used_tokens,
            free_tokens,
            token_count_kind: TokenCountKind::Estimated,
            pressure,
            categories: top,
            last_compaction,
        }
    }
}

/// `used` against the fold threshold and the hard window. Deterministic.
fn compute_pressure(
    used: u64,
    context_window: Option<u64>,
    compact_at: Option<u64>,
) -> ContextPressure {
    if let Some(window) = context_window
        && window > 0
        && used >= window
    {
        return ContextPressure::Critical;
    }
    if let Some(threshold) = compact_at
        && threshold > 0
    {
        if used >= threshold {
            return ContextPressure::Critical;
        }
        if used >= threshold * 8 / 10 {
            return ContextPressure::Warning;
        }
    }
    ContextPressure::Normal
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::ToolCallId;

    fn model() -> ModelRef {
        ModelRef::new("deepseek", "deepseek-chat")
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
        let acc = ContextAccounting::compute(model(), &[], &[], Some(128_000), Some(64_000), None);
        assert_eq!(acc.used_tokens, 0);
        assert!(acc.categories.is_empty());
        assert_eq!(acc.free_tokens, Some(128_000));
        assert_eq!(acc.pressure, ContextPressure::Normal);
        assert_eq!(acc.token_count_kind, TokenCountKind::Estimated);
    }

    #[test]
    fn system_only_session_counts_system() {
        let messages = vec![text(Role::System, "you are an agent")];
        let acc =
            ContextAccounting::compute(model(), &messages, &[], Some(128_000), Some(64_000), None);
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
                role: Role::Assistant,
                content: vec![
                    text(Role::Assistant, "reading").content[0].clone(),
                    tool_call("read_file", "c1", serde_json::json!({"path": "a"})),
                ],
            },
            Message {
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
            &messages,
            &tools,
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

    #[test]
    fn large_tool_result_is_visible_in_context_breakdown() {
        let small = "hello";
        let large = "y".repeat(40_000);
        let messages = vec![
            text(Role::System, "you are an agent"),
            text(Role::User, "inspect"),
            Message {
                role: Role::Assistant,
                content: vec![tool_call(
                    "run_command",
                    "c1",
                    serde_json::json!({"cmd": "test"}),
                )],
            },
            Message {
                role: Role::Tool,
                content: vec![tool_result("c1", &large)],
            },
            Message {
                role: Role::Assistant,
                content: vec![text(Role::Assistant, small).content[0].clone()],
            },
        ];
        let acc =
            ContextAccounting::compute(model(), &messages, &[], Some(128_000), Some(64_000), None);
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
            role: Role::Tool,
            content: vec![tool_result("never-seen", "body")],
        }];
        let acc = ContextAccounting::compute(model(), &messages, &[], None, None, None);
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
        let acc = ContextAccounting::compute(model(), &messages, &[], None, None, None);
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
        let acc = ContextAccounting::compute(model(), &[], &tools, None, None, None);
        let defs = category(&acc, "tool_definitions");
        assert!(defs.tokens > 0);
        assert_eq!(acc.used_tokens, defs.tokens);
    }

    #[test]
    fn pressure_tracks_the_fold_threshold_deterministically() {
        let win = Some(128_000u32);
        let fold = Some(64_000u32);
        // Below 80% of the fold threshold → normal.
        assert_eq!(
            compute_pressure(40_000, win.map(u64::from), fold.map(u64::from)),
            ContextPressure::Normal
        );
        // 80%..100% → warning.
        assert_eq!(
            compute_pressure(55_000, win.map(u64::from), fold.map(u64::from)),
            ContextPressure::Warning
        );
        // At/above the fold threshold → critical.
        assert_eq!(
            compute_pressure(64_000, win.map(u64::from), fold.map(u64::from)),
            ContextPressure::Critical
        );
        // At/above the hard window → critical even with no fold threshold.
        assert_eq!(
            compute_pressure(128_000, win.map(u64::from), None),
            ContextPressure::Critical
        );
    }

    #[test]
    fn free_tokens_is_window_minus_used_and_never_underflows() {
        let messages = vec![text(Role::User, &"x".repeat(1_000_000))];
        let acc = ContextAccounting::compute(model(), &messages, &[], Some(128_000), None, None);
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
        let acc = ContextAccounting::compute(model(), &messages, &[], None, None, None);
        assert_eq!(acc.context_window_tokens, None);
        assert_eq!(acc.free_tokens, None);
    }

    #[test]
    fn cjk_text_is_weighted_by_character_not_four_bytes() {
        let cjk = vec![text(Role::User, &"修".repeat(1000))];
        let ascii = vec![text(Role::User, &"a".repeat(3000))];
        let cjk_acc = ContextAccounting::compute(model(), &cjk, &[], None, None, None);
        let ascii_acc = ContextAccounting::compute(model(), &ascii, &[], None, None, None);
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
