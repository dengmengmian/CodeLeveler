//! The one mapping from a collaboration axis to execution semantics.
//!
//! `sessions.collaboration` is the durable product fact. This module is the
//! only place that fact becomes behavior: which engine turn runs (and therefore
//! which `TurnProfile`), whether the goal terminal contract applies, whether the
//! capability surface loses authoring, and whether an answer is the axis's own
//! terminal. Headless `run`, the interactive submit router, resume and the CLI's
//! exit-code projection all read this instead of deciding per caller — so a
//! session row can never be one axis while its executor runs another profile.

use leveler_lifecycle::CollaborationMode;

/// Execution semantics of one collaboration axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollaborationExecution {
    /// Goal TurnProfile: `update_goal` is the only completion authority, a
    /// quiet turn continues the goal, and authoring follows the permission
    /// profile.
    Goal,
    /// Chat TurnProfile: one conversational turn whose text is the whole
    /// terminal fact (`Answered`), with authoring per the permission profile.
    Chat,
    /// Chat TurnProfile with authoring removed from the capability surface: a
    /// read-only planning turn with no completion authority.
    Plan,
}

impl CollaborationExecution {
    /// The axis → execution mapping. The only `match` over the three axes.
    pub fn of(collaboration: CollaborationMode) -> Self {
        match collaboration {
            CollaborationMode::Goal => Self::Goal,
            CollaborationMode::Chat => Self::Chat,
            CollaborationMode::Plan => Self::Plan,
        }
    }

    /// Decode a persisted wire value, then map it. The decode is the same one
    /// every reader of the session row uses, so no caller invents a fallback.
    pub(crate) fn of_wire(collaboration: &str) -> Self {
        Self::of(collaboration_mode(collaboration))
    }

    /// Whether this axis runs the Goal TurnProfile and its `update_goal`
    /// terminal contract.
    pub fn runs_goal_lifecycle(self) -> bool {
        matches!(self, Self::Goal)
    }

    /// Whether authoring is removed from the capability surface.
    pub fn read_only(self) -> bool {
        matches!(self, Self::Plan)
    }

    /// Whether an `Answered` stop is this axis's own terminal rather than an
    /// unresolved goal. Chat and Plan exist to answer; a Goal that only
    /// answered has not been resolved.
    pub fn answer_is_the_terminal(self) -> bool {
        !self.runs_goal_lifecycle()
    }
}

/// Decode a persisted collaboration value. Case-insensitive, and an
/// unrecognized value falls back to Chat — never to a silent Goal upgrade.
pub(crate) fn collaboration_mode(value: &str) -> CollaborationMode {
    match value.to_ascii_lowercase().as_str() {
        "goal" => CollaborationMode::Goal,
        "plan" => CollaborationMode::Plan,
        _ => CollaborationMode::Chat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every axis resolves to exactly one execution profile, and the three
    /// facts callers rely on cannot drift apart.
    #[test]
    fn each_axis_has_one_execution_semantics() {
        assert_eq!(
            CollaborationExecution::of(CollaborationMode::Goal),
            CollaborationExecution::Goal
        );
        assert_eq!(
            CollaborationExecution::of(CollaborationMode::Chat),
            CollaborationExecution::Chat
        );
        assert_eq!(
            CollaborationExecution::of(CollaborationMode::Plan),
            CollaborationExecution::Plan
        );

        assert!(CollaborationExecution::Goal.runs_goal_lifecycle());
        assert!(!CollaborationExecution::Goal.read_only());
        assert!(!CollaborationExecution::Goal.answer_is_the_terminal());

        assert!(!CollaborationExecution::Chat.runs_goal_lifecycle());
        assert!(!CollaborationExecution::Chat.read_only());
        assert!(CollaborationExecution::Chat.answer_is_the_terminal());

        assert!(!CollaborationExecution::Plan.runs_goal_lifecycle());
        assert!(CollaborationExecution::Plan.read_only());
        assert!(CollaborationExecution::Plan.answer_is_the_terminal());
    }

    /// The wire decode is one rule for every reader: case-insensitive, unknown
    /// goes to Chat (never a silent Goal).
    #[test]
    fn wire_decode_is_case_insensitive_and_never_upgrades_to_goal() {
        assert_eq!(collaboration_mode("goal"), CollaborationMode::Goal);
        assert_eq!(collaboration_mode("Goal"), CollaborationMode::Goal);
        assert_eq!(collaboration_mode("plan"), CollaborationMode::Plan);
        assert_eq!(collaboration_mode("Plan"), CollaborationMode::Plan);
        assert_eq!(collaboration_mode("chat"), CollaborationMode::Chat);
        assert_eq!(collaboration_mode(""), CollaborationMode::Chat);
        assert_eq!(collaboration_mode("nonsense"), CollaborationMode::Chat);
    }
}
