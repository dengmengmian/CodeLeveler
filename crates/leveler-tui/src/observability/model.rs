//! Presentation state for `/trace`. The payload is a protocol query result.

use leveler_client_protocol::{
    CommandId, ObservationClass, UiObservabilityLoaded, UiObservationRow,
};

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

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "1 Overview",
            Self::Trace => "2 Trace",
            Self::Requests => "3 Requests",
            Self::Tools => "4 Tools",
            Self::Agents => "5 Agents",
            Self::Recovery => "6 Recovery",
        }
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
    Verify,
    Agents,
    Recovery,
    Errors,
}

impl TraceFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Model => "Model",
            Self::Tools => "Tools",
            Self::Verify => "Verify",
            Self::Agents => "Agents",
            Self::Recovery => "Recovery",
            Self::Errors => "Errors",
        }
    }

    pub fn next(self) -> Self {
        use TraceFilter::*;
        match self {
            All => Model,
            Model => Tools,
            Tools => Verify,
            Verify => Agents,
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
            Self::Verify => row.class == ObservationClass::Verify,
            Self::Agents => row.class == ObservationClass::Agent,
            Self::Recovery => row.class == ObservationClass::Recovery,
            Self::Errors => row.status == "fail",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TraceView {
    pub loaded: Option<UiObservabilityLoaded>,
    /// Query this `/trace` view currently owns. Foreign / stale
    /// `ObservabilityLoaded` events are ignored.
    pub pending_query_id: Option<CommandId>,
    pub tab: TraceTab,
    pub filter: TraceFilter,
    pub selected: usize,
    pub inspect: bool,
}

impl TraceView {
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
