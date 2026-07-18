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

/// Which tool definitions the model sees each round.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurface {
    /// Core tools only; expand_tools can grow the surface mid-session.
    Core,
    /// Full built-in registry (historical default).
    #[default]
    Full,
}

impl ToolSurface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Full => "full",
        }
    }
}

impl FromStr for ToolSurface {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "core" => Self::Core,
            "full" => Self::Full,
            other => {
                return Err(UnknownVariant {
                    kind: "tool surface",
                    value: other.to_string(),
                });
            }
        })
    }
}

/// Counters for continuous-use / latency hard gates (S0/S3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepthUseMetrics {
    /// Post-turn answer_audit generate invocations this drive.
    pub answer_audit_invocations: u32,
    /// Extra model generates caused by harness tax (audit + recovery nudges).
    pub extra_model_calls: u32,
    /// Successful PlanUpdated emissions.
    pub plan_updated: u32,
    /// True if a mutation tool ran before any plan was registered.
    pub first_write_before_plan: bool,
    /// Times a mutation was blocked for missing structured plan.
    pub plan_first_write_blocked: u32,
    /// Coarse model-token spend for this drive (input+output when known).
    #[serde(default)]
    pub model_tokens: u64,
}

impl DepthUseMetrics {
    /// Hint for epoch accumulation when detailed usage is sparse.
    pub fn model_tokens_hint(&self) -> u64 {
        self.model_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_balanced_chat() {
        assert_eq!(WorkProfile::default(), WorkProfile::Balanced);
        assert_eq!(CollaborationMode::default(), CollaborationMode::Chat);
        assert_eq!(ToolSurface::default(), ToolSurface::Full);
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
