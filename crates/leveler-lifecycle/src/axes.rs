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
    /// The axis an ORDINARY interactive session is created with.
    ///
    /// A person sitting in an interactive client asked for a conversation;
    /// the goal lifecycle is asked for explicitly (`/goal <task>`, the
    /// `--collaboration goal` entry points, a wire request that states it) or
    /// resumed from the session row. This is the ONE statement of that product
    /// intent: the terminal, the Web host and the Desktop bridge all consume
    /// it, so no two interactive clients can disagree about the axis of a new
    /// session — which they did: the same product fact was restated per client,
    /// and the Web and Desktop silently created **Goal** sessions while their
    /// own UI said `chat`.
    ///
    /// [`CollaborationMode::default`] is deliberately NOT this value: it is the
    /// product default for a coding session (a headless `leveler run`, or a wire
    /// request that omits the field), and it stays `Goal` so an omission cannot
    /// silently downgrade a goal run to a conversation.
    pub const fn interactive_session() -> Self {
        Self::Chat
    }

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
