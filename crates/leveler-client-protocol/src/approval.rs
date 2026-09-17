//! Approval request projection for the UI.
//!
//! The runtime's `leveler_execution::ApprovalRequest` carries paths and a risk
//! level; this is the render-ready view the approval overlay shows .
//! The decision type ([`ApprovalDecision`]) and id ([`ApprovalId`]) are reused
//! from the runtime unchanged.

use serde::{Deserialize, Serialize};

use leveler_core::{ApprovalId, ClarificationId};

/// A pending permission request, projected for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiApprovalRequest {
    pub id: ApprovalId,
    /// The tool requesting permission (e.g. `run_command`).
    pub tool: String,
    /// A one-line summary of what will happen.
    pub summary: String,
    /// The concrete command, when the tool is `run_command`.
    pub command: Option<String>,
    /// Human-readable risk bullets (paths touched, network, etc.).
    pub risks: Vec<String>,
    /// The tool call this request is holding, when the runtime knows it.
    ///
    /// A UI needs it to tell "announced, waiting for you" apart from "running":
    /// the call has an event on screen already, and without an id the only way
    /// to find its row would be to guess at the latest running call. `None` for
    /// a request that is not about one specific call (a standing
    /// `request_permissions`), and for a session recorded before this field.
    #[serde(default)]
    pub call_id: Option<String>,
    /// Whether "always allow" would persist a standing permission rule for
    /// this action. When `false` (consent tools such as `save_agent` or
    /// `remember`, actions with no safe rule shape, or a request recorded
    /// before this field) the runtime could only honour it for this turn,
    /// so a client must not offer it.
    #[serde(default)]
    pub always_persists: bool,
}

/// How one question in a clarification is answered.
///
/// The kind is explicit rather than inferred from `options` being empty: a
/// single-choice question with no options is unanswerable, while a text
/// question is the only shape whose answer is typed. A client that guessed
/// would silently turn a missing option list into a free-text prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ClarificationQuestionKind {
    /// Exactly one option (or a free-text answer when `allow_other` is set).
    #[default]
    Single,
    /// Zero or more options.
    Multi,
    /// A free-text answer.
    Text,
}

/// One question of a clarification interaction (spec §35).
///
/// A clarification is a set of questions the user answers in one sitting; the
/// client shows them as tabs so only one is on screen at a time. `header` is
/// the short tab label, `question` the full prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiClarificationQuestion {
    /// Short label for the question's tab. Empty means "derive one from
    /// `question`" — a display concern the client owns.
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub kind: ClarificationQuestionKind,
    /// Candidate answers for `single` / `multi`. Empty for `text`.
    #[serde(default)]
    pub options: Vec<String>,
    /// Offer a trailing free-text entry ("其他…") next to the options.
    #[serde(default)]
    pub allow_other: bool,
    /// Minimum number of picks for a `multi` question (0 = none required).
    #[serde(default)]
    pub min_choices: u32,
    /// Maximum number of picks for a `multi` question (`None` = the option
    /// count).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_choices: Option<u32>,
}

/// A mid-task clarification the agent needs answered (spec §35).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UiClarificationRequest {
    pub id: ClarificationId,
    /// The interaction's headline, and the whole prompt for a request that
    /// predates `questions`.
    pub question: String,
    /// Candidate answers, when the model offered a choice.
    pub options: Vec<String>,
    /// The questions to answer in one interaction. Empty for a legacy
    /// single-question request: clients then render `question`/`options` as
    /// one question, so an older runtime keeps working against a newer client
    /// and vice versa.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<UiClarificationQuestion>,
}

impl UiClarificationRequest {
    /// A request with no structured questions (the legacy single-question
    /// shape). Used by tests and by clients that build a request by hand.
    pub fn single(id: ClarificationId, question: impl Into<String>, options: Vec<String>) -> Self {
        Self {
            id,
            question: question.into(),
            options,
            questions: Vec::new(),
        }
    }
}

/// A live control request included in a reconnect snapshot. Only requests with
/// an in-process waiter are projected; interrupted turns never resurrect stale
/// buttons after a process restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", content = "request", rename_all = "snake_case")]
pub enum UiPendingInteraction {
    Approval(UiApprovalRequest),
    Clarification(UiClarificationRequest),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request recorded before `questions` existed still parses: clients and
    /// runtimes of different vintages must interoperate.
    #[test]
    fn a_legacy_single_question_request_has_no_questions() {
        let json = r#"{"id":"c1","question":"which file?","options":["a.rs"]}"#;
        let req: UiClarificationRequest = serde_json::from_str(json).expect("legacy payload");
        assert!(req.questions.is_empty());
        assert_eq!(req.question, "which file?");
        assert_eq!(req.options, vec!["a.rs".to_string()]);
        // And it does not grow a `questions` key when written back.
        let out = serde_json::to_string(&req).unwrap();
        assert!(!out.contains("questions"), "{out}");
    }

    #[test]
    fn a_multi_question_request_roundtrips_its_shapes() {
        let req = UiClarificationRequest {
            id: ClarificationId::new("c1"),
            question: "需要你的选择".into(),
            options: Vec::new(),
            questions: vec![
                UiClarificationQuestion {
                    header: "数据策略".into(),
                    question: "数据策略怎么定？".into(),
                    kind: ClarificationQuestionKind::Single,
                    options: vec!["保留 demo fallback".into(), "删除全部 mock".into()],
                    allow_other: true,
                    min_choices: 0,
                    max_choices: None,
                },
                UiClarificationQuestion {
                    header: "验证范围".into(),
                    question: "需要跑哪些验证？".into(),
                    kind: ClarificationQuestionKind::Multi,
                    options: vec!["单元测试".into(), "TUI 测试".into()],
                    allow_other: false,
                    min_choices: 1,
                    max_choices: Some(2),
                },
                UiClarificationQuestion {
                    header: "补充要求".into(),
                    question: "还有其他要求吗？".into(),
                    kind: ClarificationQuestionKind::Text,
                    options: Vec::new(),
                    allow_other: false,
                    min_choices: 0,
                    max_choices: None,
                },
            ],
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: UiClarificationRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
        // The kinds are on the wire by name, not by position.
        assert!(json.contains(r#""kind":"single""#), "{json}");
        assert!(json.contains(r#""kind":"multi""#), "{json}");
        assert!(json.contains(r#""kind":"text""#), "{json}");
    }

    #[test]
    fn an_unset_kind_defaults_to_single() {
        let json = r#"{"id":"c1","question":"h","options":[],"questions":[{"question":"q","options":["a"]}]}"#;
        let req: UiClarificationRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.questions[0].kind, ClarificationQuestionKind::Single);
        assert_eq!(req.questions[0].header, "");
        assert!(!req.questions[0].allow_other);
        assert_eq!(req.questions[0].min_choices, 0);
        assert_eq!(req.questions[0].max_choices, None);
    }
}
