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
    /// Original request before continuation amendments. Older rows omit it;
    /// then `text` itself remains the primary objective.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    /// User-authored constraints added by later continuation turns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub amendments: Vec<String>,
}

impl ObjectiveAnchor {
    pub fn new(text: impl Into<String>, source: ObjectiveSource) -> Self {
        let text = text.into();
        Self {
            primary: Some(text.clone()),
            text,
            version: 1,
            source,
            amendments: Vec::new(),
        }
    }

    pub fn from_user_message(text: impl Into<String>) -> Self {
        Self::new(text, ObjectiveSource::ThisTurnUser)
    }

    pub fn from_session_goal(text: impl Into<String>) -> Self {
        Self::new(text, ObjectiveSource::SessionGoal)
    }

    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Add one durable user amendment. The effective `text` remains the
    /// compatibility view consumed by existing contracts and prompts.
    pub fn amend(&mut self, amendment: impl Into<String>) {
        let amendment = amendment.into();
        let amendment = amendment.trim();
        if amendment.is_empty() || self.amendments.iter().any(|value| value == amendment) {
            return;
        }
        if self.primary.is_none() {
            self.primary = Some(self.text.clone());
        }
        self.amendments.push(amendment.to_string());
        self.version = self.version.saturating_add(1);
        self.text.push_str("\n\nContinuation amendment:\n");
        self.text.push_str(amendment);
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

    #[test]
    fn amendments_are_versioned_durable_and_idempotent() {
        let mut anchor = ObjectiveAnchor::from_user_message("configure Ark");
        anchor.amend("keep the chat profile");
        anchor.amend("keep the chat profile");

        assert_eq!(anchor.version, 2);
        assert_eq!(anchor.primary.as_deref(), Some("configure Ark"));
        assert_eq!(anchor.amendments, ["keep the chat profile"]);
        assert!(anchor.text().contains("configure Ark"));
        assert!(anchor.text().contains("keep the chat profile"));

        let back: ObjectiveAnchor =
            serde_json::from_value(serde_json::to_value(&anchor).unwrap()).unwrap();
        assert_eq!(back, anchor);
    }
}
