//! Text helpers shared by manifest rendering and response processing.

/// Truncate `value` to at most `max_bytes`, backing up to a UTF-8 character
/// boundary so the result is always valid text.
pub fn truncate_str(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.get(..end).unwrap_or("")
}

/// Largest index at or before `index` that lies on a UTF-8 character boundary.
pub fn floor_char_boundary(value: &str, index: usize) -> usize {
    let mut end = index.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Response processing uses this to check the model produced structured output.
pub fn has_markdown_headers(text: &str) -> bool {
    text.contains("## ") || text.contains("# ")
}

/// True when the response is the NO_REPLY marker in any case or separator variant: `"no reply"`, `"no_reply"`, `"No-Reply"`, `"NO REPLY"`.
///
/// Strips all non-alphanumeric characters, lowercases, and checks if the remainder is exactly `"noreply"`.
pub fn is_no_reply(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    normalized == "noreply"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_no_reply() {
        assert!(is_no_reply("NO_REPLY"));
        assert!(is_no_reply("no reply"));
        assert!(is_no_reply("No-Reply"));
        assert!(is_no_reply("noreply"));
        assert!(!is_no_reply("no reply needed"));
        assert!(!is_no_reply("I have things to store"));
    }

    #[test]
    fn test_has_markdown_headers() {
        assert!(has_markdown_headers("## Topic"));
        assert!(has_markdown_headers("# Title\n\nBody"));
        assert!(has_markdown_headers("preamble\n\n## Topic"));
        assert!(!has_markdown_headers("plain text without headers"));
        assert!(!has_markdown_headers("#hashtag without space"));
    }
}
