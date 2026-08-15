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

/// Largest char-boundary index `<= i` (clamped to `s.len()`).
///
/// Stable stand-in for the unstable `str::floor_char_boundary`.
pub fn floor_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut i = i;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char-boundary index `>= i` (clamped to `s.len()`).
///
/// Stable stand-in for the unstable `str::ceil_char_boundary`.
pub fn ceil_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Keep the head of `s`, budgeted by bytes, and append `marker` when truncated.
///
/// - Returns `s` unchanged (no marker) when `s.len() <= max_bytes`.
/// - The cut point backs off to a char boundary, so at most `max_bytes` bytes
///   of content are kept; the marker is appended *on top of* the budget.
/// - `max_bytes == 0` yields the marker alone (for non-empty `s`).
pub fn truncate_head_bytes(s: &str, max_bytes: usize, marker: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let end = floor_char_boundary(s, max_bytes);
    format!("{}{marker}", &s[..end])
}

/// Keep the tail of `s`, budgeted by bytes, and prepend `marker` when truncated
/// (for output whose interesting part is at the end, e.g. compiler errors).
///
/// - Returns `s` unchanged (no marker) when `s.len() <= max_bytes`.
/// - The cut point advances to a char boundary, so at most `max_bytes` bytes
///   of content are kept; the marker is prepended *on top of* the budget.
/// - `max_bytes == 0` yields the marker alone (for non-empty `s`).
pub fn truncate_tail_bytes(s: &str, max_bytes: usize, marker: &str) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let start = ceil_char_boundary(s, s.len() - max_bytes);
    format!("{marker}{}", &s[start..])
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

/// Redact secrets inside a serialized JSON document without ever touching its
/// structure: parse, scrub only string VALUES with [`redact_secrets`], and
/// re-serialize. Keys, discriminators, numbers, and delimiters are never
/// rewritten, so the output is valid JSON (and schema-compatible with the
/// input) by construction.
///
/// R007 F2: running the plain-text scrubber over an already-serialized event
/// payload let a keyword match swallow the JSON string's own closing quote and
/// the structural bytes after it, persisting unparseable rows that made the
/// whole session unreplayable. Durable JSON planes must use this entry point;
/// the plain [`redact_secrets`] remains for non-JSON text (e.g. artifacts).
///
/// An input that is not valid JSON is a caller bug, not data to "best-effort"
/// scrub: the error is returned so the write boundary can fail loud instead of
/// persisting an unvalidated payload.
pub fn redact_secrets_json(payload: &str) -> Result<String, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(payload)?;
    redact_json_string_values(&mut value);
    serde_json::to_string(&value)
}

fn redact_json_string_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            let redacted = redact_secrets(s);
            if redacted != *s {
                *s = redacted;
            }
        }
        serde_json::Value::Array(items) => {
            items.iter_mut().for_each(redact_json_string_values);
        }
        serde_json::Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                // The serialized-text scrubber used to see `"password":"…"` as
                // one text span and redact by KEY name; after moving to
                // value-level scrubbing that signal lives here: a sensitive key
                // redacts its whole string value.
                match entry {
                    serde_json::Value::String(s) if is_sensitive_key(key) && !s.is_empty() => {
                        *s = "[REDACTED]".to_string();
                    }
                    _ => redact_json_string_values(entry),
                }
            }
        }
        _ => {}
    }
}

/// Key names whose string values are credentials by contract. Mirrors (and
/// slightly widens, toward over-redaction) the serialized-text coverage of
/// [`redact_kv_assignments`] + the Authorization-header scrubber.
fn is_sensitive_key(key: &str) -> bool {
    const KEY_MARKERS: &[&str] = &[
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "access-token",
        "secret",
        "password",
        "passwd",
        "passphrase",
        "auth_token",
        "auth-token",
        "refresh_token",
        "refresh-token",
        "private_key",
        "private-key",
        "signing_key",
        "signing-key",
        "authorization",
    ];
    let lower = key.to_ascii_lowercase();
    KEY_MARKERS.iter().any(|marker| lower.contains(marker))
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

    /// R007 F2 accident shape: the plain scrubber over a serialized event
    /// payload swallowed the JSON string's closing quote and the structural
    /// bytes after it. The JSON-aware entry point must keep the document
    /// parseable while still removing the secret.
    #[test]
    fn json_redaction_survives_the_r007_accident_payload() {
        // Byte-exact R007 shape: the narration ENDS with `PASSWORD:` at the
        // JSON string boundary. The serialized-text scrubber mistook the
        // document's own closing quote for the value's opening quote and ate
        // `},"` — producing exactly the corrupt row R007 persisted. There is
        // no secret here at all; structure must survive untouched.
        let payload = r#"{"payload":{"text":"Secrets 标签页有 \"Add new\" 按钮。点击添加 secret PASSWORD:"},"type":"assistant_message"}"#;
        assert!(
            serde_json::from_str::<serde_json::Value>(&redact_secrets(payload)).is_err(),
            "pre-fix scrubber must corrupt this shape or the accident no longer reproduces"
        );
        let redacted = redact_secrets_json(payload).expect("redaction must not fail");
        let value: serde_json::Value =
            serde_json::from_str(&redacted).expect("redacted payload must stay valid JSON");
        assert_eq!(value["type"], "assistant_message", "discriminator intact");
        assert_eq!(
            value["payload"]["text"],
            "Secrets 标签页有 \"Add new\" 按钮。点击添加 secret PASSWORD:",
            "no secret present — text must be unchanged"
        );

        // Companion shape WITH a real secret value inside the narration: the
        // secret must go, the structure must stay.
        let with_secret = r#"{"payload":{"text":"add secret PASSWORD:\"hunter2-secret-value\""},"type":"assistant_message"}"#;
        let redacted = redact_secrets_json(with_secret).unwrap();
        serde_json::from_str::<serde_json::Value>(&redacted).expect("valid JSON");
        assert!(!redacted.contains("hunter2-secret-value"), "{redacted}");
    }

    /// Table coverage: secrets in every structural position must be removed
    /// while the document stays valid JSON and keeps its shape (§39).
    #[test]
    fn json_redaction_is_structure_preserving_across_positions() {
        let cases = [
            // plain string value
            r#"{"note":"password: swordfish-77"}"#,
            // nested object
            r#"{"a":{"b":{"c":"Authorization: Bearer tok-1234567890abcdef"}}}"#,
            // array element
            r#"{"lines":["ok","secret: deep-array-value-1","ok"]}"#,
            // escaped-JSON-like content INSIDE a string value
            r#"{"text":"config was {\"token\":\"tok-abcdefabcdef\"}"}"#,
            // markdown/shell narration
            r#"{"out":"$ export API_TOKEN=sk-abcdefghijklmnop1234\ndone"}"#,
            // multiline with colon/comma/braces around the secret
            "{\"text\":\"line1\\npassword: p@ss,with{braces}\\nline3\"}",
            // unicode neighbours
            r#"{"text":"密码 password: 秘密值abc 之后"}"#,
            // context-snapshot-like nesting
            r#"{"payload":{"messages":[{"content":[{"text":"secret: snap-value-9"}]}]},"type":"context_snapshot"}"#,
        ];
        for payload in cases {
            let before: serde_json::Value = serde_json::from_str(payload).unwrap();
            let redacted = redact_secrets_json(payload)
                .unwrap_or_else(|e| panic!("redaction failed for {payload}: {e}"));
            let after: serde_json::Value = serde_json::from_str(&redacted)
                .unwrap_or_else(|e| panic!("invalid JSON after redaction of {payload}: {e}"));
            assert_eq!(
                shape_of(&before),
                shape_of(&after),
                "structure changed for {payload}: {redacted}"
            );
        }
    }

    /// Numbers, bools, nulls, and keys are never rewritten.
    #[test]
    fn json_redaction_touches_only_string_values() {
        let payload =
            r#"{"password_attempts":3,"ok":true,"none":null,"secret":"real-secret-value"}"#;
        let redacted = redact_secrets_json(payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&redacted).unwrap();
        assert_eq!(value["password_attempts"], 3);
        assert_eq!(value["ok"], true);
        assert!(value["none"].is_null());
        assert!(value.get("secret").is_some(), "key must survive");
        assert!(!redacted.contains("real-secret-value"), "{redacted}");
    }

    /// Non-JSON input is a caller bug at a durable JSON boundary: fail loud,
    /// never best-effort scrub.
    #[test]
    fn json_redaction_rejects_non_json_input() {
        assert!(redact_secrets_json("password: plain, not json").is_err());
        assert!(redact_secrets_json(r#"{"truncated":"#).is_err());
    }

    /// Structural fingerprint: object keys + container shapes, ignoring string
    /// leaf contents (which redaction may rewrite).
    fn shape_of(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Object(map) => {
                let inner: Vec<String> = map
                    .iter()
                    .map(|(k, v)| format!("{k}:{}", shape_of(v)))
                    .collect();
                format!("{{{}}}", inner.join(","))
            }
            serde_json::Value::Array(items) => {
                let inner: Vec<String> = items.iter().map(shape_of).collect();
                format!("[{}]", inner.join(","))
            }
            serde_json::Value::String(_) => "s".into(),
            serde_json::Value::Number(_) => "n".into(),
            serde_json::Value::Bool(_) => "b".into(),
            serde_json::Value::Null => "0".into(),
        }
    }

    #[test]
    fn floor_boundary_clamps_down_and_ceil_clamps_up() {
        let s = "aé"; // 'a' at 0, 'é' occupies bytes 1..3.
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(ceil_char_boundary(s, 2), 3);
        // Already on a boundary: unchanged.
        assert_eq!(floor_char_boundary(s, 1), 1);
        assert_eq!(ceil_char_boundary(s, 1), 1);
        // Past the end: clamp to len.
        assert_eq!(floor_char_boundary(s, 10), 3);
        assert_eq!(ceil_char_boundary(s, 10), 3);
        // Empty string.
        assert_eq!(floor_char_boundary("", 0), 0);
        assert_eq!(ceil_char_boundary("", 5), 0);
    }

    #[test]
    fn head_truncation_keeps_prefix_and_appends_marker() {
        assert_eq!(truncate_head_bytes("abcdef", 3, "…"), "abc…");
    }

    #[test]
    fn head_truncation_leaves_short_input_alone() {
        assert_eq!(truncate_head_bytes("abc", 3, "…"), "abc");
        assert_eq!(truncate_head_bytes("abc", 10, "…"), "abc");
        assert_eq!(truncate_head_bytes("", 0, "…"), "");
    }

    #[test]
    fn head_truncation_backs_off_to_char_boundary() {
        // 'é' is 2 bytes; cutting at 3 lands mid-'é' (bytes 2..4).
        let s = "aéé";
        let out = truncate_head_bytes(s, 3, "…");
        assert_eq!(out, "aé…");
        // First char multibyte, cut inside it: keep nothing, marker only.
        let out = truncate_head_bytes("é9", 1, "…");
        assert_eq!(out, "…");
    }

    #[test]
    fn head_truncation_with_zero_budget_yields_marker_only() {
        assert_eq!(truncate_head_bytes("abc", 0, "[cut]"), "[cut]");
    }

    #[test]
    fn head_truncation_marker_longer_than_budget_is_kept_whole() {
        // The marker is on top of max_bytes by contract, never trimmed.
        assert_eq!(
            truncate_head_bytes("abcdef", 2, "[truncated]"),
            "ab[truncated]"
        );
    }

    #[test]
    fn tail_truncation_keeps_suffix_and_prepends_marker() {
        assert_eq!(truncate_tail_bytes("abcdef", 3, "…"), "…def");
    }

    #[test]
    fn tail_truncation_leaves_short_input_alone() {
        assert_eq!(truncate_tail_bytes("abc", 3, "…"), "abc");
        assert_eq!(truncate_tail_bytes("abc", 10, "…"), "abc");
        assert_eq!(truncate_tail_bytes("", 0, "…"), "");
    }

    #[test]
    fn tail_truncation_advances_to_char_boundary() {
        // "ééa": bytes 0..2, 2..4, 4..5. max=3 → cut at 2, already a boundary.
        assert_eq!(truncate_tail_bytes("ééa", 3, "…"), "…éa");
        // max=2 on "aé9": cut at 1 lands mid-'é'? No — 'é' is 1..3, cut = 3-2 = 1
        // is a boundary. Use "éé": max=3 → cut at 1, mid first 'é', advance to 2.
        assert_eq!(truncate_tail_bytes("éé", 3, "…"), "…é");
    }

    #[test]
    fn tail_truncation_with_zero_budget_yields_marker_only() {
        assert_eq!(truncate_tail_bytes("abc", 0, "[cut]"), "[cut]");
    }
}
