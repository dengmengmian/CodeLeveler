//! Read-only observability DTOs. Projection of durable runtime facts — not a
//! second event log and not a write model.

use serde::{Deserialize, Serialize};

use leveler_core::SessionId;

/// Presentation class for a meaningful durable event. Unknown tools map to
/// [`Self::Tool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ObservationClass {
    Model,
    Read,
    Search,
    Edit,
    Shell,
    Tool,
    Verify,
    Agent,
    Recovery,
    System,
    Terminal,
}

impl ObservationClass {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Model => "MODEL",
            Self::Read => "READ",
            Self::Search => "SEARCH",
            Self::Edit => "EDIT",
            Self::Shell => "SHELL",
            Self::Tool => "TOOL",
            Self::Verify => "VERIFY",
            Self::Agent => "AGENT",
            Self::Recovery => "RECOVERY",
            Self::System => "SYSTEM",
            Self::Terminal => "TERMINAL",
        }
    }
}

/// Classify a tool name for observatory presentation. Unknown → [`ObservationClass::Tool`].
pub fn classify_tool(name: &str) -> ObservationClass {
    match name {
        "read_file" | "list_files" | "read_symbol" | "view_image" => ObservationClass::Read,
        "grep" | "find_files" | "find_symbol" | "find_references" | "blast_radius" => {
            ObservationClass::Search
        }
        "apply_patch" | "edit_file" | "write_file" => ObservationClass::Edit,
        "run_command" | "shell_command" => ObservationClass::Shell,
        _ if name.starts_with("mcp__") => ObservationClass::Tool,
        _ => ObservationClass::Tool,
    }
}

/// One bounded, safe trace row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiObservationRow {
    pub sequence: i64,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub class: ObservationClass,
    pub title: String,
    #[serde(default)]
    pub target: String,
    /// running | ok | fail | info
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Durable event type tag (`tool_call_finished`, …).
    pub event_type: String,
    /// Safe inspect fields only (no raw args, no prompt, no secrets).
    #[serde(default)]
    pub fields: Vec<UiObservationField>,
}

/// One inspect key/value. Avoids tuple arrays in the JSON schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiObservationField {
    pub key: String,
    pub value: String,
}

/// Session-level observation header + aggregates from durable stores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiSessionObservation {
    pub session_id: SessionId,
    pub goal: String,
    pub repository: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub model: String,
    pub work_profile: String,
    pub collaboration: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sequence: Option<i64>,
    pub request_count: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_latency_ms: Option<u64>,
    pub request_failures: u32,
    pub request_retries: u32,
    pub tool_started: u32,
    pub tool_finished: u32,
    pub verification_runs: u32,
    /// The latest verdict the runtime actually recorded: `passed`, `failed`,
    /// `not_run` (nothing ever started), or `unavailable` (started and never
    /// reached a verdict). A count of runs is not a verdict.
    pub verification: String,
    pub compact_count: u32,
    pub subagent_started: u32,
    /// Wall clock from the session's first to its latest durable timestamp.
    /// `None` when either end is unparseable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Prompt tokens the provider served from cache — a SUBSET of
    /// `input_tokens`. `None` when no request recorded the figure at all; that
    /// is an absence of measurement, and summing it as zero would report a
    /// cache miss nobody observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Summed over the requests that carry a price. `None` when none does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
    /// Spend split by who did it: the root session, its children, and the
    /// total. Present whenever there is a request to attribute.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<UiLaneAccounting>,
}

/// What one lane of a session spent. `lane` is `main`, `children` or `total`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiLaneAccounting {
    pub lane: String,
    pub requests: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
}

/// One durable model-request row (no prompt/body).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiRequestObservation {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    pub retry_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
    /// The sub-agent that made the call; absent for the root session's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub created_at: String,
}

/// Per-tool aggregate for the **whole session**, independent of the event
/// window. Paired on `(call_id, agent_id)`; duration only from a matching
/// start+finish. Unfinished starts are not success and do not invent duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiToolAggregate {
    pub name: String,
    pub class: ObservationClass,
    pub calls: u32,
    /// Finished with `is_error = false`.
    #[serde(default)]
    pub succeeded: u32,
    /// Finished with `is_error = true`.
    pub failed: u32,
    /// `tool_call_started` with no matching `tool_call_finished`.
    #[serde(default)]
    pub unfinished: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_ms: Option<u64>,
}

/// Durable sub-agent / reviewer observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiAgentObservation {
    pub id: String,
    pub nickname: String,
    pub role: String,
    pub status: String,
    #[serde(default)]
    pub summary: String,
}

/// Recovery facts that are already durable and safe to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiRecoveryObservation {
    pub interrupted_turns: u32,
    pub workspace_snapshots: u32,
    pub review_stages: Vec<String>,
}

/// Identity-based relation (never inferred from wall-clock proximity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiEventRelation {
    pub sequence: i64,
    /// pair_start | pair_end | same_turn | same_agent
    pub kind: String,
    pub label: String,
}

/// Bounded event window + related observation slices. Current and historical
/// sessions use this same payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiObservabilityLoaded {
    pub session: UiSessionObservation,
    pub window: Vec<UiObservationRow>,
    pub window_from: i64,
    pub window_to: i64,
    pub requests: Vec<UiRequestObservation>,
    pub tools: Vec<UiToolAggregate>,
    pub agents: Vec<UiAgentObservation>,
    pub recovery: UiRecoveryObservation,
    #[serde(default)]
    pub relations: Vec<UiEventRelation>,
}

/// Hard caps for a single query. Larger sessions paginate via `center_seq`.
pub const OBSERVABILITY_WINDOW_MAX: u32 = 100;
pub const OBSERVABILITY_REQUESTS_MAX: usize = 200;
