//! Active task objective for one execution boundary (turn / goal continue).

use serde::{Deserialize, Serialize};

/// Where the current objective text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveSource {
    /// This turn's primary user message (Chat content turn).
    #[default]
    ThisTurnUser,
    /// Session / RunGoal goal text.
    SessionGoal,
    /// Engine continue_active_goal restatement of the still-active goal.
    ContinueActive,
}

/// Canonical user-visible objective for contract, nudges, audit, and closeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectiveAnchor {
    pub text: String,
    pub version: u32,
    pub source: ObjectiveSource,
}

impl ObjectiveAnchor {
    pub fn new(text: impl Into<String>, source: ObjectiveSource) -> Self {
        Self {
            text: text.into(),
            version: 1,
            source,
        }
    }

    pub fn from_user_message(text: impl Into<String>) -> Self {
        Self::new(text, ObjectiveSource::ThisTurnUser)
    }

    pub fn from_session_goal(text: impl Into<String>) -> Self {
        Self::new(text, ObjectiveSource::SessionGoal)
    }

    pub fn for_continue(text: impl Into<String>, version: u32) -> Self {
        Self {
            text: text.into(),
            version: version.max(1),
            source: ObjectiveSource::ContinueActive,
        }
    }

    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_json() {
        let a = ObjectiveAnchor::from_user_message("update docs");
        let v = serde_json::to_value(&a).unwrap();
        let back: ObjectiveAnchor = serde_json::from_value(v).unwrap();
        assert_eq!(back.text, "update docs");
        assert_eq!(back.source, ObjectiveSource::ThisTurnUser);
    }
}
