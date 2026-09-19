//! Continuation intent: the one place that decides whether a user message is
//! asking to *continue the current logical task* rather than to start a new
//! request.
//!
//! This is deliberately a small, explicit phrase table plus a separator rule,
//! not an NLP classifier. The runtime still owns the authoritative question —
//! whether a resumable task actually exists — via
//! [`crate::ClientCommand::ResumeTask`]; this module only answers "did the
//! user express continuation intent, and what did they add on top of it?".
//!
//! The separator requirement is what keeps a new request that merely *starts*
//! with a continuation word ("继续之前的退款审计任务") from being read as a
//! continuation of the task in front of it: a continuation phrase must be the
//! whole message, or be followed by punctuation/whitespace and then an
//! amendment.

/// Continuation intent lifted off a user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuation {
    /// The phrase that matched (`继续`, `go on`, …), in its original casing.
    pub phrase: String,
    /// What the user added after the phrase, when anything follows a separator.
    ///
    /// `None` means "continue as before"; `Some` is an amendment that must be
    /// honored *in addition to* the original objective, never instead of it.
    pub amendment: Option<String>,
}

/// Phrases that mean "keep going with the task already in front of us".
///
/// Longest first, so `继续做` wins over `继续` and `接着做` over `接着`.
const CONTINUATION_PHRASES: &[&str] = &[
    "继续刚才的",
    "接着刚才的",
    "continue",
    "go on",
    "resume",
    "继续做",
    "接着做",
    "继续",
    "接着",
];

/// Characters that may separate a continuation phrase from an amendment.
///
/// Punctuation only — deliberately not whitespace. `resume the other repo` and
/// `继续之前的任务` are new requests that merely begin with a continuation word;
/// requiring punctuation keeps them out while `继续，但是先不要跑测试` and
/// `resume, but skip tests` stay continuations.
fn is_separator(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。' | '、' | '；' | '：' | '！' | '？' | ',' | '.' | ';' | ':' | '!' | '?' | '~'
    )
}

/// Parse continuation intent from a user message.
///
/// Returns `None` when the text is an ordinary request. Matching is
/// case-insensitive for the ASCII phrases.
pub fn parse_continuation(text: &str) -> Option<Continuation> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lowered = trimmed.to_ascii_lowercase();
    for phrase in CONTINUATION_PHRASES {
        let phrase_lower = phrase.to_ascii_lowercase();
        if !lowered.starts_with(&phrase_lower) {
            continue;
        }
        // Match on the original text's byte length, which is identical here:
        // the lowering only touches ASCII, and these phrases are ASCII/CJK.
        let rest = &trimmed[phrase.len()..];
        if rest.is_empty() {
            return Some(Continuation {
                phrase: (*phrase).to_string(),
                amendment: None,
            });
        }
        let mut chars = rest.chars();
        let first = chars.next()?;
        if !is_separator(first) {
            // e.g. "继续之前的任务": a new request, not a continuation.
            continue;
        }
        let amendment = chars.as_str().trim();
        if amendment.is_empty() {
            // Trailing punctuation only ("继续。"): still a plain continuation.
            return Some(Continuation {
                phrase: (*phrase).to_string(),
                amendment: None,
            });
        }
        return Some(Continuation {
            phrase: (*phrase).to_string(),
            amendment: Some(amendment.to_string()),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amendment(text: &str) -> Option<String> {
        parse_continuation(text).and_then(|c| c.amendment)
    }

    #[test]
    fn bare_continuations_match_without_amendment() {
        for text in [
            "继续",
            "接着",
            "继续做",
            "继续刚才的",
            "continue",
            "Go on",
            "resume",
        ] {
            let parsed = parse_continuation(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(parsed.amendment, None, "{text}");
        }
    }

    #[test]
    fn amendments_are_lifted_off_after_a_separator() {
        assert_eq!(
            amendment("继续，但是先不要跑测试").as_deref(),
            Some("但是先不要跑测试")
        );
        assert_eq!(
            amendment("继续, but skip tests").as_deref(),
            Some("but skip tests")
        );
        assert_eq!(
            amendment("go on: don't touch README").as_deref(),
            Some("don't touch README")
        );
        assert_eq!(amendment("继续。").as_deref(), None);
    }

    #[test]
    fn a_new_request_that_starts_with_a_continuation_word_is_not_a_continuation() {
        for text in [
            "继续之前的退款审计任务",
            "接着写一个新模块",
            "resume the other repo",
            "继续检查语法",
            "continue the other task",
        ] {
            assert!(parse_continuation(text).is_none(), "{text}");
        }
    }

    #[test]
    fn ordinary_messages_are_untouched() {
        assert!(parse_continuation("你好，帮我看看这个 bug").is_none());
        assert!(parse_continuation("").is_none());
    }
}
