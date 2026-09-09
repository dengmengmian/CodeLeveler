//! Product session axes (collaboration × work profile × tool surface).
//!
//! Pure types only — no I/O, no engine back-edges. Wiring lands in later waves.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::UnknownVariant;

/// How the user wants to collaborate this session.
///
/// Default is **Chat** (`ModeKind::Default`): ordinary TUI/CLI
/// conversation ends when the model answers; `update_goal` is not required.
/// Goal is opt-in (`/goal`, `--collaboration goal`) for long delivery runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationMode {
    /// Free-form chat; no update_goal requirement (product default).
    #[default]
    Chat,
    /// Read-only planning; confirm → goal (W5).
    Plan,
    /// Drive until complete/blocked via update_goal.
    Goal,
}

impl CollaborationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Plan => "plan",
            Self::Goal => "goal",
        }
    }
}

impl FromStr for CollaborationMode {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "chat" => Self::Chat,
            "plan" => Self::Plan,
            "goal" => Self::Goal,
            other => {
                return Err(UnknownVariant {
                    kind: "collaboration mode",
                    value: other.to_string(),
                });
            }
        })
    }
}

/// Cost / discipline profile for a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkProfile {
    /// Reduced tool surface; lighter gates.
    Economy,
    /// Default production path.
    #[default]
    Balanced,
    /// Strong process evidence on complete.
    Delivery,
}

impl WorkProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Economy => "economy",
            Self::Balanced => "balanced",
            Self::Delivery => "delivery",
        }
    }
}

impl FromStr for WorkProfile {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "economy" => Self::Economy,
            "balanced" => Self::Balanced,
            "delivery" => Self::Delivery,
            other => {
                return Err(UnknownVariant {
                    kind: "work profile",
                    value: other.to_string(),
                });
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_balanced_chat() {
        assert_eq!(WorkProfile::default(), WorkProfile::Balanced);
        assert_eq!(CollaborationMode::default(), CollaborationMode::Chat);
    }

    #[test]
    fn work_profile_round_trips() {
        for p in [
            WorkProfile::Economy,
            WorkProfile::Balanced,
            WorkProfile::Delivery,
        ] {
            assert_eq!(WorkProfile::from_str(p.as_str()).unwrap(), p);
            let v = serde_json::to_value(p).unwrap();
            let back: WorkProfile = serde_json::from_value(v).unwrap();
            assert_eq!(back, p);
        }
    }
}
