//! Presentation state for `/trace`. The payload is a protocol query result.

use leveler_client_protocol::{ObservationClass, UiObservabilityLoaded, UiObservationRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraceTab {
    #[default]
    Overview,
    Trace,
    Requests,
    Tools,
    Agents,
    Recovery,
}

impl TraceTab {
    pub const ALL: [Self; 6] = [
        Self::Overview,
        Self::Trace,
        Self::Requests,
        Self::Tools,
        Self::Agents,
        Self::Recovery,
    ];

    /// The digit is the key that selects this tab, so it stays; the word
    /// beside it is UI vocabulary and follows the locale.
    pub fn label(self, t: &crate::i18n::UiText) -> String {
        let (digit, word) = match self {
            Self::Overview => ('1', t.trace_tab_overview),
            Self::Trace => ('2', t.trace_tab_trace),
            Self::Requests => ('3', t.trace_tab_requests),
            Self::Tools => ('4', t.trace_tab_tools),
            Self::Agents => ('5', t.trace_tab_agents),
            Self::Recovery => ('6', t.trace_tab_recovery),
        };
        format!("{digit} {word}")
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn from_digit(c: char) -> Option<Self> {
        match c {
            '1' => Some(Self::Overview),
            '2' => Some(Self::Trace),
            '3' => Some(Self::Requests),
            '4' => Some(Self::Tools),
            '5' => Some(Self::Agents),
            '6' => Some(Self::Recovery),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraceFilter {
    #[default]
    All,
    Model,
    Tools,
    Agents,
    Recovery,
    Errors,
}

impl TraceFilter {
    /// The display name. The filter's semantics live in the enum, never in
    /// this string, so translating it cannot change what is filtered.
    pub fn label(self, t: &crate::i18n::UiText) -> &'static str {
        match self {
            Self::All => t.trace_filter_all,
            Self::Model => t.trace_filter_model,
            Self::Tools => t.trace_filter_tools,
            Self::Agents => t.trace_filter_agents,
            Self::Recovery => t.trace_filter_recovery,
            Self::Errors => t.trace_filter_errors,
        }
    }

    pub fn next(self) -> Self {
        use TraceFilter::*;
        match self {
            All => Model,
            Model => Tools,
            Tools => Agents,
            Agents => Recovery,
            Recovery => Errors,
            Errors => All,
        }
    }

    pub fn matches(self, row: &UiObservationRow) -> bool {
        match self {
            Self::All => true,
            Self::Model => row.class == ObservationClass::Model,
            Self::Tools => matches!(
                row.class,
                ObservationClass::Read
                    | ObservationClass::Search
                    | ObservationClass::Edit
                    | ObservationClass::Shell
                    | ObservationClass::Tool
            ),
            Self::Agents => row.class == ObservationClass::Agent,
            Self::Recovery => row.class == ObservationClass::Recovery,
            Self::Errors => row.status == "fail",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TraceView {
    pub loaded: Option<UiObservabilityLoaded>,
    /// Correlation/refresh discipline for the queries this `/trace` view owns.
    /// Foreign, replayed and superseded `ObservabilityLoaded` answers are
    /// ignored; refreshes coalesce to one query in flight.
    pub track: crate::query_track::QueryTrack,
    pub tab: TraceTab,
    pub filter: TraceFilter,
    pub selected: usize,
    pub inspect: bool,
    /// When the newest query went out. Only the live cadence reads it, so a
    /// page that is closed or idle owes nothing on the clock.
    pub last_query_at: Option<std::time::Instant>,
}

impl TraceView {
    /// Whether a page on screen should ask again purely because time passed.
    ///
    /// The runtime appends rows no client event reports (a long tool's progress
    /// ticks), so a live page cannot rely on fact events alone. The gate keeps
    /// it to one outstanding query at this cadence.
    pub fn live_refresh_due(&self, now: std::time::Instant) -> bool {
        self.track.in_flight_id().is_none()
            && self
                .last_query_at
                .is_none_or(|at| now.saturating_duration_since(at) >= super::LIVE_REFRESH_INTERVAL)
    }
    pub fn filtered(&self) -> Vec<&UiObservationRow> {
        self.loaded
            .as_ref()
            .map(|l| l.window.iter().filter(|r| self.filter.matches(r)).collect())
            .unwrap_or_default()
    }

    pub fn selected_row(&self) -> Option<&UiObservationRow> {
        let rows = self.filtered();
        rows.get(self.selected).copied()
    }

    pub fn move_sel(&mut self, delta: isize) {
        let n = self.filtered().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, (n - 1) as isize) as usize;
    }

    pub fn clamp(&mut self) {
        let n = self.filtered().len();
        if n == 0 {
            self.selected = 0;
            self.inspect = false;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }
}
