//! Durable usage reporting: the ONE projection of what the runtime's own
//! model-call ledger spent.
//!
//! The numbers here come from `model_requests` rows the runtime wrote when each
//! provider call settled — never from a client's transcript, never from the
//! current context size. Two properties are therefore structural rather than
//! promised:
//!
//! - **Replay cannot double count.** A report is an aggregate over durable rows,
//!   so re-reading it (reconnect, resume, a second client) recomputes the same
//!   figures. Nothing is accumulated on the wire.
//! - **Unmeasured is not zero.** A row whose provider reported no usage, a row
//!   with no configured price, and a figure nobody recorded stay `None`/counted
//!   as unknown instead of being summed as zero.
//!
//! Token relationships, in this order of containment:
//!
//! ```text
//! input_tokens ⊇ cached_input_tokens      (a cache hit is part of the prompt)
//! output_tokens ⊇ reasoning_tokens        (thinking is part of the completion)
//! total_tokens = input_tokens + output_tokens
//! ```
//!
//! Cached input and reasoning are therefore **never** added to the total again:
//! they are slices, and [`UiUsageReport::total_tokens`] is the only total.

use serde::{Deserialize, Serialize};

/// The window a usage report covers.
///
/// The runtime — not the client — resolves a scope into an explicit
/// `[from, to)` range and echoes it in the report, so two surfaces showing the
/// same scope can never disagree about its boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UiUsageScope {
    /// The session's latest turn (the running one, else the last one).
    Turn,
    /// Everything the current session ever spent.
    #[default]
    Session,
    /// The caller's local calendar day.
    Today,
    /// The caller's local day six days ago through now.
    Last7Days,
    /// The caller's local day twenty-nine days ago through now.
    Last30Days,
    /// Every durable call this runtime's ledger holds.
    All,
}

impl UiUsageScope {
    /// Every scope, in the order the UI cycles them.
    pub const ALL: [Self; 6] = [
        Self::Turn,
        Self::Session,
        Self::Today,
        Self::Last7Days,
        Self::Last30Days,
        Self::All,
    ];

    /// The stable wire token (also the persisted/presented key).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Session => "session",
            Self::Today => "today",
            Self::Last7Days => "last7_days",
            Self::Last30Days => "last30_days",
            Self::All => "all",
        }
    }

    /// How many local calendar days back the scope reaches, counted from
    /// today. `None` = the scope is not a day span (turn/session/all).
    pub fn day_span(self) -> Option<u32> {
        match self {
            Self::Today => Some(1),
            Self::Last7Days => Some(7),
            Self::Last30Days => Some(30),
            Self::Turn | Self::Session | Self::All => None,
        }
    }

    /// Whether the scope is bounded by a time range rather than by a session.
    pub fn is_time_range(self) -> bool {
        self.day_span().is_some()
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let i = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// One provider/model's share of a report. Requests are grouped by the
/// provider key and the model id actually sent on the wire, so switching models
/// mid-session splits here instead of blending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiModelUsage {
    pub provider: String,
    pub model: String,
    /// Provider calls in this group, retries included — each is a billed call.
    pub requests: u32,
    /// Calls in this group whose provider reported no usage at all.
    pub unmeasured_requests: u32,
    pub input_tokens: u64,
    /// Subset of `input_tokens`; `None` when no row in the group recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: u64,
    /// Subset of `output_tokens`; `None` when no row in the group recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Sum over the group's priced rows; `None` when none of them had a price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
    /// Rows in the group that carried a price.
    pub priced_requests: u32,
}

impl UiModelUsage {
    /// `input + output` for this group. Cached input and reasoning are slices
    /// of those two figures, never additions.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Prompt tokens billed at the uncached rate, when the split was measured.
    pub fn uncached_input_tokens(&self) -> Option<u64> {
        self.cached_input_tokens
            .map(|cached| self.input_tokens.saturating_sub(cached))
    }

    /// Whether the group has any provider-reported usage at all.
    pub fn has_measured_usage(&self) -> bool {
        self.total_tokens() > 0
    }
}

/// What one scope spent, with the range the runtime resolved it to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiUsageReport {
    pub scope: UiUsageScope,
    /// Resolved lower bound (inclusive), RFC 3339. `None` = unbounded below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Resolved upper bound (exclusive), RFC 3339. `None` = the report covers
    /// everything up to the moment it was generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// When the runtime computed this report (RFC 3339).
    pub generated_at: String,
    /// Calls in scope, retries included.
    pub requests: u32,
    /// Calls in scope whose provider reported no usage. They are counted as
    /// requests but contribute no tokens: an absence of measurement.
    pub unmeasured_requests: u32,
    pub input_tokens: u64,
    /// Subset of `input_tokens`, summed over rows that recorded a cache figure.
    /// `None` when no row in scope recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: u64,
    /// Subset of `output_tokens`, summed over rows that reported a breakdown.
    /// `None` when no row in scope did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Sum over rows that carried a price. `None` when none did — the honest
    /// answer, never `$0.00`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
    /// Rows whose cost is part of `cost_usd_micros`.
    pub priced_requests: u32,
    /// Per provider/model breakdown, largest spender first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<UiModelUsage>,
}

impl UiUsageReport {
    /// `input + output`. The only total: cached input and reasoning are slices
    /// of these two figures and must never be added again.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Prompt tokens billed at the uncached rate, when the split was measured.
    pub fn uncached_input_tokens(&self) -> Option<u64> {
        self.cached_input_tokens
            .map(|cached| self.input_tokens.saturating_sub(cached))
    }

    /// Whether the scope holds no calls at all (the honest empty state).
    pub fn is_empty(&self) -> bool {
        self.requests == 0
    }

    /// Whether calls exist but none of them reported usage.
    pub fn has_no_measured_usage(&self) -> bool {
        self.requests > 0 && self.total_tokens() == 0
    }

    /// Whether some calls in scope have no price, so `cost_usd_micros` (if any)
    /// is a FLOOR rather than the scope's complete cost.
    pub fn cost_is_partial(&self) -> bool {
        self.priced_requests < self.requests
    }

    /// The flat cost summary a caller may print: `None` when nothing in scope
    /// could be priced.
    pub fn cost_usd(&self) -> Option<f64> {
        self.cost_usd_micros
            .map(|micros| micros as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(requests: u32, input: u64, output: u64, cached: Option<u64>) -> UiUsageReport {
        UiUsageReport {
            scope: UiUsageScope::Session,
            from: None,
            to: None,
            generated_at: "2026-01-01T00:00:00Z".into(),
            requests,
            unmeasured_requests: 0,
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            reasoning_tokens: None,
            cost_usd_micros: None,
            priced_requests: 0,
            models: Vec::new(),
        }
    }

    #[test]
    fn total_counts_input_and_output_only() {
        // Cached input is a slice of input; adding it again would inflate the
        // total by the cache hit.
        let r = report(1, 1_000, 200, Some(900));
        assert_eq!(r.total_tokens(), 1_200);
        assert_eq!(r.uncached_input_tokens(), Some(100));
    }

    #[test]
    fn unmeasured_usage_is_not_an_empty_scope() {
        let measured_empty = report(0, 0, 0, None);
        assert!(measured_empty.is_empty());
        assert!(!measured_empty.has_no_measured_usage());

        let unmeasured = report(2, 0, 0, None);
        assert!(!unmeasured.is_empty());
        assert!(unmeasured.has_no_measured_usage());
    }

    #[test]
    fn partial_cost_is_reported_as_partial() {
        let mut r = report(3, 10, 5, None);
        r.cost_usd_micros = Some(1_500);
        r.priced_requests = 1;
        assert!(r.cost_is_partial());
        assert_eq!(r.cost_usd(), Some(0.0015));

        r.priced_requests = 3;
        assert!(!r.cost_is_partial());
    }

    #[test]
    fn scopes_cycle_and_name_their_day_span() {
        assert_eq!(UiUsageScope::default(), UiUsageScope::Session);
        assert_eq!(UiUsageScope::Today.day_span(), Some(1));
        assert_eq!(UiUsageScope::Last7Days.day_span(), Some(7));
        assert_eq!(UiUsageScope::Last30Days.day_span(), Some(30));
        assert_eq!(UiUsageScope::Turn.day_span(), None);
        assert!(!UiUsageScope::All.is_time_range());
        assert_eq!(UiUsageScope::All.next(), UiUsageScope::Turn);
        assert_eq!(UiUsageScope::Turn.prev(), UiUsageScope::All);
    }

    #[test]
    fn scope_wire_tokens_are_stable() {
        for scope in UiUsageScope::ALL {
            let json = serde_json::to_string(&scope).unwrap();
            assert_eq!(json, format!("\"{}\"", scope.as_str()));
            let back: UiUsageScope = serde_json::from_str(&json).unwrap();
            assert_eq!(back, scope);
        }
    }
}
