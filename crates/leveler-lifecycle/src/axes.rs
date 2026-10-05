//! Product collaboration axis. Tool exposure belongs to goal capabilities.
//!
//! Pure types only — no I/O, no engine back-edges. Wiring lands in later waves.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::UnknownVariant;

/// How the user wants to collaborate this session.
///
/// Default is **Goal**: CodeLeveler is a Coding Agent, so a newly created
/// session drives its work to an explicit goal and an ordinary assistant
/// final is an `Answered` continuation, not a terminal. Chat is an explicit
/// choice (`/collab chat`, `--collaboration chat`) for conversation that ends
/// when the model answers and never requires `update_goal`.
///
/// This is the one authoritative default for a new session. An omitted wire
/// field, a fresh [`Application`](https://docs.rs/leveler-app) and a CLI run
/// with no `--collaboration` all resolve here — no client and no prompt
/// inspection decides the mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationMode {
    /// Free-form chat; no update_goal requirement. Explicit, never the default.
    Chat,
    /// Read-only planning; confirm → goal (W5).
    Plan,
    /// Drive until complete/blocked via update_goal (product default).
    #[default]
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_collaboration_is_goal() {
        assert_eq!(
            CollaborationMode::default(),
            CollaborationMode::Goal,
            "a new Coding Session defaults to goal, not chat"
        );
    }
}
