//! The exit-confirmation dialog's model.
//!
//! It is a normal [`SelectionModel`] plus the affected executions the runtime
//! reported. The rows are presentation data copied from [`crate::exit`]; the
//! reducer keeps the authoritative impact separately so a re-check never reads
//! what is merely on screen.

use crate::exit::ExitImpact;
use crate::overlay::selection::SelectionModel;

/// Options in the order they are shown. The keys are stable identities, never
/// display positions, so a moved row cannot redirect a confirmation.
pub const EXIT_OPTION_STOP: &str = "stop";
pub const EXIT_OPTION_BACKGROUND: &str = "background";
pub const EXIT_OPTION_STAY: &str = "stay";

#[derive(Debug, Clone)]
pub struct ExitConfirmOverlay {
    pub model: SelectionModel,
    /// The one authoritative impact this dialog was opened from. "Continue"
    /// hands this exact set back for the pre-exit re-check.
    pub impact: ExitImpact,
    /// The terminal is too small for the full explanation. The options keep
    /// their labels and any disabled reason; only the supporting descriptions
    /// and some breathing room are dropped, so the title, the affected work
    /// and the safe choice stay on screen together.
    pub compact: bool,
}
