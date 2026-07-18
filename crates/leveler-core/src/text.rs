//! Terminal-oriented text cleanup for tool output and TUI previews.
//!
//! Commands like vitest/npm emit ANSI color codes. If we only blank control
//! characters, the ESC is dropped and the CSI body (`[32m`) remains as
//! garbage in the transcript. Strip full sequences first, then neutralize
//! residual controls (keeping newlines).

/// Remove ANSI/VT escape sequences and non-newline control characters so the
/// result is safe to show in a cell-based TUI or store as a tool preview.
pub fn sanitize_terminal_output(input: &str) -> String {
    let without_escapes = strip_ansi_escapes(input);
    neutralize_controls(&without_escapes)
}

/// Strip CSI / OSC / simple ESC sequences. Also drops orphaned CSI tails of the
/// form `[0-9;]*[A-Za-z]` that appear when ESC was already replaced by a space.
fn strip_ansi_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    // CSI: intermediate/params, then a final byte in 0x40..=0x7E.
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: terminated by BEL or ST (ESC \).
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{07}' {
                            break;
                        }
                        if n == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    // Two-character ESC sequences (e.g. ESC c) — drop the next.
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        // Orphan CSI: previous pass replaced ESC with space → " [32m" or "[32m".
        if c == '[' {
            let mut look = chars.clone();
            let mut saw_param = false;
            let mut final_byte = None;
            for n in look.by_ref() {
                if n.is_ascii_digit() || n == ';' {
                    saw_param = true;
                    continue;
                }
                if n.is_ascii_alphabetic() && saw_param {
                    final_byte = Some(n);
                }
                break;
            }
            if final_byte.is_some() {
                // Consume the params + final letter from the real iterator.
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn neutralize_controls(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c == '\n' {
                c
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Redact common secret shapes before persistence (messages, events, artifacts).
///
/// Heuristic and deliberately conservative: prefer over-redacting known key
/// forms over storing raw credentials. Config-file plaintext provider keys are
/// a supported product feature; this only protects *runtime records*.
pub fn redact_secrets(input: &str) -> String {
    let with_headers = redact_authorization_values(input);
    let with_keys = redact_prefixed_keys(&with_headers);
    redact_kv_assignments(&with_keys)
}

/// `Authorization: Bearer <token>`, JSON `"Authorization":"Bearer …"`, etc.
fn redact_authorization_values(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(rel) = find_ci(rest, "authorization") {
        out.push_str(&rest[..rel]);
        let after = &rest[rel..];
        // "authorization" + optional closing quote + ws* + ":" + ws* +
        // optional "Bearer" + ws* + value (optionally quoted).
        let mut cursor = "authorization".len();
        cursor = skip_ws(after, cursor);
        // JSON key closing quote: "Authorization"
        if after[cursor..].starts_with('"') || after[cursor..].starts_with('\'') {
            cursor += 1;
            cursor = skip_ws(after, cursor);
        }
        if !after[cursor..].starts_with(':') {
            // Not a header/kv form — emit one char and continue scanning.
            let step = after.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            out.push_str(&after[..step]);
            rest = &after[step..];
            continue;
        }
        cursor += 1;
        cursor = skip_ws(after, cursor);
        // Optional opening quote on the value.
        let value_quote = after[cursor..]
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'');
        if value_quote.is_some() {
            cursor += 1;
            cursor = skip_ws(after, cursor);
        }
        // Optional Bearer (char-based — never byte-slice across UTF-8).
        if starts_with_ci_ascii(after, cursor, "bearer") {
            cursor += "bearer".len();
            cursor = skip_ws(after, cursor);
        }
        out.push_str(&after[..cursor]);
        let value_len = secret_value_len(&after[cursor..], value_quote);
        if value_len > 0 {
            out.push_str("[REDACTED]");
            cursor += value_len;
        }
        // Optional closing quote on the value.
        if let Some(q) = value_quote
            && after[cursor..].starts_with(q)
        {
            out.push(q);
            cursor += 1;
        }
        rest = &after[cursor..];
    }
    out.push_str(rest);
    out
}

/// `sk-…` (len ≥ 16 body) and `AKIA` + 16 alnum.
fn redact_prefixed_keys(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i..].starts_with("sk-") {
            let start = i;
            i += 3;
            let body_start = i;
            while i < input.len() {
                let ch = input[i..].chars().next().unwrap();
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            let body_len = i - body_start;
            let prev_ok = start == 0
                || !input[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            if prev_ok && body_len >= 16 {
                out.push_str("[REDACTED]");
                continue;
            }
            out.push_str(&input[start..i]);
            continue;
        }
        let token_prefix = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"]
            .into_iter()
            .find(|prefix| input[i..].starts_with(prefix));
        if let Some(prefix) = token_prefix {
            let start = i;
            i += prefix.len();
            let body_start = i;
            while i < input.len() {
                let ch = input[i..].chars().next().unwrap();
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            let prev_ok = start == 0
                || !input[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            if prev_ok && i - body_start >= 8 {
                out.push_str("[REDACTED]");
                continue;
            }
            out.push_str(&input[start..i]);
            continue;
        }
        if input[i..].starts_with("AKIA") && i + 20 <= input.len() {
            let candidate = &input[i..i + 20];
            if candidate
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                let prev_ok = i == 0
                    || !input[..i]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_ascii_alphanumeric());
                let next_ok = i + 20 >= input.len()
                    || !input[i + 20..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric());
                if prev_ok && next_ok {
                    out.push_str("[REDACTED]");
                    i += 20;
                    continue;
                }
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `api_key=…`, `"api_key":"…"`, `access_token: "…"`, etc.
fn redact_kv_assignments(input: &str) -> String {
    const KEYS: &[&str] = &[
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "access-token",
        "secret_key",
        "secret-key",
        "client_secret",
        "client-secret",
        "password",
        "passwd",
        "auth_token",
        "auth-token",
        "refresh_token",
        "refresh-token",
        "private_key",
        "private-key",
        "signing_key",
        "signing-key",
    ];
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let mut hit: Option<&str> = None;
        for key in KEYS {
            if lower[i..].starts_with(key) {
                hit = Some(*key);
                break;
            }
        }
        let Some(key) = hit else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        };
        if i > 0
            && input[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let key_end = i + key.len();
        let mut cursor = key_end;
        // JSON: "api_key"  — closing quote after the key name.
        cursor = skip_ws(input, cursor);
        if input[cursor..].starts_with('"') || input[cursor..].starts_with('\'') {
            cursor += 1;
            cursor = skip_ws(input, cursor);
        }
        let sep = input[cursor..].chars().next();
        if sep != Some(':') && sep != Some('=') {
            out.push_str(&input[i..key_end]);
            i = key_end;
            continue;
        }
        cursor += 1;
        cursor = skip_ws(input, cursor);
        let value_quote = input[cursor..]
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'');
        if value_quote.is_some() {
            cursor += 1;
            cursor = skip_ws(input, cursor);
        }
        out.push_str(&input[i..cursor]);
        let value_len = secret_value_len(&input[cursor..], value_quote);
        if value_len > 0 {
            out.push_str("[REDACTED]");
            cursor += value_len;
        }
        if let Some(q) = value_quote
            && input[cursor..].starts_with(q)
        {
            out.push(q);
            cursor += 1;
        }
        i = cursor;
    }
    out
}

fn skip_ws(s: &str, mut cursor: usize) -> usize {
    while let Some(c) = s[cursor..].chars().next() {
        if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
            cursor += c.len_utf8();
        } else {
            break;
        }
    }
    cursor
}

/// Char-safe case-insensitive ASCII prefix check at byte offset `at`.
fn starts_with_ci_ascii(s: &str, at: usize, prefix: &str) -> bool {
    let mut rest = s[at..].chars();
    for expected in prefix.chars() {
        match rest.next() {
            Some(c) if c.eq_ignore_ascii_case(&expected) => {}
            _ => return false,
        }
    }
    true
}

fn secret_value_len(s: &str, value_quote: Option<char>) -> usize {
    match value_quote {
        Some(q) => {
            let mut len = 0;
            let mut escaped = false;
            for c in s.chars() {
                if !escaped && (c == q || c == '\n' || c == '\r') {
                    break;
                }
                len += c.len_utf8();
                escaped = !escaped && c == '\\';
            }
            len
        }
        None => s
            .chars()
            .take_while(|c| {
                !c.is_whitespace()
                    && *c != '"'
                    && *c != '\''
                    && *c != ','
                    && *c != ';'
                    && *c != '}'
                    && *c != ']'
                    && *c != '\\'
            })
            .map(|c| c.len_utf8())
            .sum(),
    }
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let needle_lower = needle.to_ascii_lowercase();
    let hay_lower = haystack.to_ascii_lowercase();
    hay_lower.find(&needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_color_codes() {
        let raw = "\u{1b}[1m\u{1b}[30m\u{1b}[46m RUN \u{1b}[49m\u{1b}[39m\u{1b}[22m";
        let clean = sanitize_terminal_output(raw);
        assert_eq!(clean.trim(), "RUN");
        assert!(!clean.contains('['), "left CSI body: {clean:?}");
        assert!(!clean.contains('\u{1b}'));
    }

    #[test]
    fn strips_orphan_csi_after_esc_was_blanked() {
        // What TUI used to produce by only neutralizing controls.
        let half = " [1m [30m [46m RUN [49m";
        let clean = sanitize_terminal_output(half);
        assert!(
            clean.contains("RUN") && !clean.contains("[1m") && !clean.contains("[30m"),
            "orphan CSI remains: {clean:?}"
        );
    }

    #[test]
    fn keeps_newlines_and_neutralizes_tab_cr() {
        let raw = "a\tb\rc\nd";
        let clean = sanitize_terminal_output(raw);
        assert!(clean.contains('\n'));
        assert!(!clean.contains('\t') && !clean.contains('\r'));
        assert!(clean.contains('a') && clean.contains('d'));
    }

    #[test]
    fn vitest_style_line() {
        let raw = "\u{1b}[32m✓\u{1b}[39m src/foo.test.ts \u{1b}[2m(2 tests)\u{1b}[22m";
        let clean = sanitize_terminal_output(raw);
        assert!(clean.contains('✓'));
        assert!(clean.contains("src/foo.test.ts"));
        assert!(clean.contains("2 tests"));
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains("[32m"));
    }

    #[test]
    fn redacts_sk_style_and_authorization() {
        let raw = "Authorization: Bearer sk-abcdefghijklmnop1234 and api_key=supersecretvalue99";
        let clean = redact_secrets(raw);
        assert!(!clean.contains("sk-abcdefghijklmnop1234"), "{clean}");
        assert!(!clean.contains("supersecretvalue99"), "{clean}");
        assert!(clean.contains("[REDACTED]"), "{clean}");
        assert!(clean.contains("Authorization:"), "{clean}");
    }

    #[test]
    fn redacts_json_api_key_and_authorization() {
        let raw =
            r#"{"api_key":"supersecretvalue99","Authorization":"Bearer ghp_secret_token_xyz"}"#;
        let clean = redact_secrets(raw);
        assert!(!clean.contains("supersecretvalue99"), "{clean}");
        assert!(!clean.contains("ghp_secret_token_xyz"), "{clean}");
        assert!(
            clean.contains(r#""api_key":"#) || clean.contains("api_key"),
            "{clean}"
        );
        assert!(clean.contains("[REDACTED]"), "{clean}");
    }

    #[test]
    fn redacts_entire_json_string_even_with_escaped_quote() {
        let raw = r#"{"client_secret":"before\"after","safe":"visible"}"#;
        let clean = redact_secrets(raw);
        assert!(!clean.contains("before"), "{clean}");
        assert!(!clean.contains("after"), "{clean}");
        assert!(clean.contains(r#""safe":"visible""#), "{clean}");
    }

    #[test]
    fn authorization_redact_survives_multibyte_prefix() {
        // Cursor math must not panic when non-ASCII precedes Authorization.
        let raw = "密码 Authorization: Bearer token_value_here_xx";
        let clean = redact_secrets(raw);
        assert!(!clean.contains("token_value_here_xx"), "{clean}");
        assert!(clean.contains("[REDACTED]"), "{clean}");
    }

    #[test]
    fn redacts_credential_environment_assignment_shapes() {
        let raw = concat!(
            "PASSWORD=hunter2-long-value ",
            "AUTH_TOKEN=auth-secret-value ",
            "REFRESH_TOKEN=refresh-secret-value ",
            "PRIVATE_KEY='private-secret-value'"
        );
        let clean = redact_secrets(raw);
        for secret in [
            "hunter2-long-value",
            "auth-secret-value",
            "refresh-secret-value",
            "private-secret-value",
        ] {
            assert!(!clean.contains(secret), "{clean}");
        }
    }

    #[test]
    fn redacts_common_github_token_prefixes_without_panicking_on_unicode() {
        let raw = "密钥 ghp_abcdefghijklmnop github_pat_11AA0_longTokenBody99";
        let clean = redact_secrets(raw);
        assert!(!clean.contains("ghp_abcdefghijklmnop"), "{clean}");
        assert!(
            !clean.contains("github_pat_11AA0_longTokenBody99"),
            "{clean}"
        );
        assert_eq!(clean.matches("[REDACTED]").count(), 2, "{clean}");
    }

    #[test]
    fn short_github_like_identifiers_are_not_redacted() {
        let raw = "document ghp_example and github_pat_docs";
        assert_eq!(redact_secrets(raw), raw);
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let raw = "run cargo test in src/lib.rs";
        assert_eq!(redact_secrets(raw), raw);
    }
}
