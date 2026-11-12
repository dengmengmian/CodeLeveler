//! What the user actually said.
//!
//! A requirement declaration may quote only text the user authored. The
//! objective anchor and the newest message are two such sources; a
//! clarification answer is a third. Tool results are not: the user's answer
//! reaches the model as the result of `request_user_input`, but the same shape
//! also carries command output, file contents and everything else the agent
//! produced. The runtime therefore keeps the answers it knows a person wrote,
//! with the call that asked, and authorizes a quote against that list instead
//! of against an arbitrary tool result.

use leveler_lifecycle::quote_is_in_input;

/// One answer a person gave to the model's question, with its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClarificationAnswer {
    /// Durable identity shared by the requested and answered events.
    pub id: leveler_core::ClarificationId,
    /// The tool call the answer belongs to (empty on rows written before the
    /// request carried its call identity).
    pub call_id: String,
    /// The turn that asked, when the durable row carried one.
    pub turn_id: Option<String>,
    /// The user's words, verbatim.
    pub text: String,
}

/// The text a requirement or scope declaration may quote, by source.
pub(crate) struct UserAuthority<'a> {
    objective: &'a str,
    latest_input: &'a str,
    answers: &'a [ClarificationAnswer],
    /// Answers received after the newest user input. A scope change needs the
    /// user's current words: an answer the newest message may already have
    /// reversed is not current authority, even though it remains what the
    /// person said for a requirement declaration.
    answers_after_input: &'a [ClarificationAnswer],
}

impl<'a> UserAuthority<'a> {
    pub(crate) fn new(
        objective: &'a str,
        latest_input: &'a str,
        answers: &'a [ClarificationAnswer],
        answers_after_input: &'a [ClarificationAnswer],
    ) -> Self {
        Self {
            objective,
            latest_input,
            answers,
            answers_after_input,
        }
    }

    pub(crate) fn objective(&self) -> &'a str {
        self.objective
    }

    pub(crate) fn has_answers(&self) -> bool {
        !self.answers.is_empty()
    }

    /// Whether a `commit_task` requirement quote is the user's own text: from
    /// the request that anchors the task, or from an answer to the model's
    /// question.
    pub(crate) fn authorizes_requirement(&self, quote: &str) -> bool {
        quote_is_in_input(quote, self.objective) || self.answers_authorize(quote)
    }

    /// Whether a scope-operation quote is the user's current text: from the
    /// newest message governing this window, or from an answer received after
    /// it. The objective anchor is deliberately absent — an older request
    /// cannot authorize a scope change that the newest message may already
    /// have reversed. Answers given before the newest message are absent for
    /// the same reason.
    pub(crate) fn authorizes_scope_change(&self, quote: &str) -> bool {
        quote_is_in_input(quote, self.latest_input)
            || self
                .answers_after_input
                .iter()
                .any(|answer| quote_is_in_input(quote, &answer.text))
    }

    fn answers_authorize(&self, quote: &str) -> bool {
        self.answers
            .iter()
            .any(|answer| quote_is_in_input(quote, &answer.text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(text: &str) -> ClarificationAnswer {
        ClarificationAnswer {
            id: leveler_core::ClarificationId::new("c1"),
            call_id: "call-1".into(),
            turn_id: Some("t1".into()),
            text: text.into(),
        }
    }

    #[test]
    fn an_answer_authorizes_a_requirement_but_only_a_current_answer_changes_scope() {
        let answers = vec![answer("媒体来源：外链 URL 字段")];
        let authority = UserAuthority::new("接入媒体库", "继续", &answers, &[]);
        assert!(authority.authorizes_requirement("外链 URL 字段"));
        // The answer predates the newest message ("继续"). It remains what
        // the person said, but a scope change needs words that are current.
        assert!(!authority.authorizes_scope_change("外链 URL 字段"));
        // Text the user never typed is unauthorized everywhere.
        assert!(!authority.authorizes_requirement("内网对象存储"));
        assert!(!authority.authorizes_scope_change("内网对象存储"));
        // The objective authorizes a requirement, never a scope change.
        assert!(authority.authorizes_requirement("接入媒体库"));
        assert!(!authority.authorizes_scope_change("接入媒体库"));
    }

    #[test]
    fn an_answer_received_after_the_newest_input_authorizes_a_scope_change() {
        let answers = vec![answer("标题字段使用英文")];
        let authority = UserAuthority::new("接入媒体库", "继续", &answers, &answers);
        assert!(authority.authorizes_scope_change("标题字段使用英文"));
    }

    #[test]
    fn skipped_or_absent_answers_are_not_authority() {
        // The loader only produces answered rows with text; an empty list is
        // what a skip or an unattended run leaves behind.
        let authority = UserAuthority::new("接入媒体库", "继续", &[], &[]);
        assert!(!authority.has_answers());
        assert!(!authority.authorizes_requirement("媒体来源：外链 URL 字段"));
        assert!(!authority.authorizes_scope_change("媒体来源：外链 URL 字段"));
    }

    /// An answer given before the user's newest message is not authority to
    /// change scope after it. The user chose "外链 URL" through a question,
    /// then replaced it in a normal message: the newest message governs, so a
    /// quote that exists only in the older answer must not amend the
    /// requirement back.
    #[test]
    fn an_answer_from_before_the_newest_input_cannot_authorize_a_scope_change() {
        let old = answer("媒体来源：外链 URL 字段");
        let new = ClarificationAnswer {
            id: leveler_core::ClarificationId::new("c2"),
            call_id: "call-2".into(),
            turn_id: Some("t1".into()),
            text: "改用文件上传".into(),
        };
        let answers = vec![old, new.clone()];
        let authority = UserAuthority::new(
            "接入媒体库",
            "改为文件上传，不再使用之前的来源方案",
            &answers,
            std::slice::from_ref(&new),
        );
        assert!(
            !authority.authorizes_scope_change("外链 URL 字段"),
            "the answer predates the newest message and cannot reverse it"
        );
        assert!(authority.authorizes_scope_change("文件上传"));
        // It remains what the person said, so it can still anchor a
        // requirement declaration; only scope changes need current words.
        assert!(authority.authorizes_requirement("外链 URL 字段"));
    }
}
