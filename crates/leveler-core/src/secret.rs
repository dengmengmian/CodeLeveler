//! Secret handling for text that is about to leave the harness — either
//! towards the model and the provider, or towards durable storage.
//!
//! R007 F6: secrets were redacted only on the way to the database, so a
//! credential the agent read reached the model *and the provider* in
//! plaintext; once the model paraphrased it into prose, no key/value shape
//! remained for storage-side redaction to recognise and the plaintext
//! persisted anyway. The reliable fix is upstream — a value the model never
//! receives is a value it cannot repeat.
//!
//! **This is deliberately not [`crate::redact_secrets`] moved earlier.** That
//! function is a whole-line keyword scrubber; measured against tool output it
//! rewrites ordinary source code (`let password = config.password;` became
//! `let password = [REDACTED];`) while still missing the commonest real
//! shapes (`API_PASSWORD=…`, `export TOKEN=…`, `postgres://u:pw@host`).
//! Sanitising model *input* has the opposite error budget from sanitising a
//! stored record: mangling what the agent reads breaks the product. So
//! detection here is **value-position aware** — it redacts the VALUE of a
//! sensitive assignment and leaves everything else intact, including the key,
//! so the agent can still reason about configuration structure.
//!
//! Scope is bounded on purpose. No dataflow analysis, no entropy scanning, no
//! encoded-form detection: the positions below are where credentials actually
//! arrive.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Replacement text for a detected secret value.
pub const SECRET_PLACEHOLDER: &str = "[REDACTED]";

/// Shortest value worth redacting. Below this a "secret" is almost always a
/// placeholder, a flag, or a fragment of prose, and replacing it does more
/// damage than good.
const MIN_SECRET_LEN: usize = 6;

/// Upper bound on values remembered per session, so a pathological run cannot
/// grow the registry without limit.
const MAX_REGISTERED_PER_SESSION: usize = 256;

/// One detected secret: where it sat and what the value was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedSecret {
    /// The concrete value. Never logged, never persisted.
    pub value: String,
    /// The key or context it was found under, e.g. `API_PASSWORD`. Safe to
    /// log — it is a name, not a credential.
    pub key: String,
}

/// Sanitize text that is about to become model-visible, returning the text
/// with secret VALUES replaced and the list of values that were found.
///
/// Keys, structure, code, and prose are preserved. Safe to run on arbitrary
/// tool output — source files, shell output, JSON, YAML, `.env`, logs.
pub fn sanitize_model_visible(text: &str) -> (String, Vec<DetectedSecret>) {
    if text.is_empty() {
        return (String::new(), Vec::new());
    }
    let mut found = Vec::new();
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&sanitize_line(line, &mut found));
    }
    (out, found)
}

fn sanitize_line(line: &str, found: &mut Vec<DetectedSecret>) -> String {
    let step = redact_url_userinfo(line, found);
    let step = redact_sensitive_assignments(&step, found);
    // Self-identifying key shapes (`sk-…`, `ghp_…`, `AKIA…`) need no key
    // position at all — they ARE the credential. Carried over from the
    // pre-F6 scrubber so durable coverage never regresses.
    redact_prefixed_key_shapes(&step, found)
}

/// Provider key prefixes that identify a credential by their own shape.
fn redact_prefixed_key_shapes(line: &str, found: &mut Vec<DetectedSecret>) -> String {
    let scrubbed = crate::text::redact_prefixed_keys(line);
    if scrubbed != line {
        // Recover the concrete values so they can also be registered.
        for token in line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
            let is_key = (token.starts_with("sk-") && token.len() >= 19)
                || (token.starts_with("AKIA") && token.len() == 20)
                || ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"]
                    .iter()
                    .any(|p| token.starts_with(p) && token.len() > p.len() + 7);
            if is_key {
                found.push(DetectedSecret {
                    value: token.to_string(),
                    key: "provider-key-prefix".to_string(),
                });
            }
        }
    }
    scrubbed
}

// ── URL userinfo: scheme://user:secret@host ─────────────────────────────────

fn redact_url_userinfo(line: &str, found: &mut Vec<DetectedSecret>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find("://") {
        let (head, tail) = rest.split_at(pos + 3);
        out.push_str(head);
        let authority_end = tail
            .find(|c: char| {
                c == '/' || c == '?' || c == '#' || c.is_whitespace() || c == '"' || c == '\''
            })
            .unwrap_or(tail.len());
        let authority = &tail[..authority_end];
        match (authority.find('@'), authority.find(':')) {
            (Some(at), Some(colon)) if colon < at => {
                let value = &authority[colon + 1..at];
                if value.len() >= MIN_SECRET_LEN {
                    out.push_str(&authority[..=colon]);
                    out.push_str(SECRET_PLACEHOLDER);
                    out.push_str(&authority[at..]);
                    found.push(DetectedSecret {
                        value: value.to_string(),
                        key: "url-userinfo".to_string(),
                    });
                } else {
                    out.push_str(authority);
                }
            }
            _ => out.push_str(authority),
        }
        rest = &tail[authority_end..];
    }
    out.push_str(rest);
    out
}

// ── KEY <sep> VALUE, including CLI flags and Authorization headers ──────────

fn redact_sensitive_assignments(line: &str, found: &mut Vec<DetectedSecret>) -> String {
    let code_line = looks_like_code(line);
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut idx = 0usize;
    while idx < line.len() {
        let Some((key_start, key_end)) = next_sensitive_key(line, idx) else {
            out.push_str(&line[idx..]);
            return out;
        };
        // Report the FULL key name (`SERVICE_PASSWORD`, not `PASSWORD`) —
        // it is a name, safe to log, and far more useful in diagnostics.
        let name_start = line[..key_start]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
            .last()
            .map_or(key_start, |(i, _)| i);
        let key = line[name_start..key_end].to_string();
        let mut cursor = key_end;
        // Optional closing quote on a JSON/YAML key: "password"
        if matches!(bytes.get(cursor), Some(b'"') | Some(b'\'')) {
            cursor += 1;
        }
        cursor = skip_spaces(bytes, cursor);
        // Separator: `=` or `:` … or plain whitespace for CLI flags
        // (`--password hunter2`), which only counts when the key was a flag.
        let flag_style = line[..key_start].ends_with("--") || line[..key_start].ends_with('-');
        let sep = bytes.get(cursor).copied();
        let has_sep = matches!(sep, Some(b'=') | Some(b':'));
        if has_sep {
            // `==`, `=>`, `::` are operators, never assignments.
            if matches!(bytes.get(cursor + 1), Some(b'=') | Some(b'>') | Some(b':')) {
                out.push_str(&line[idx..key_end]);
                idx = key_end;
                continue;
            }
            cursor += 1;
        } else if !(flag_style && cursor > key_end) {
            // No separator and not `--flag value`: this is a bare mention.
            out.push_str(&line[idx..key_end]);
            idx = key_end;
            continue;
        }
        cursor = skip_spaces(bytes, cursor);
        // `Authorization: Bearer <token>` — the scheme word is not the secret.
        for scheme in ["bearer ", "basic ", "token "] {
            // `is_char_boundary` guards the slice: the value may begin with a
            // multi-byte character (Chinese prose around a credential is a
            // real case), and slicing mid-character panics.
            if line.len() >= cursor + scheme.len()
                && line.is_char_boundary(cursor)
                && line.is_char_boundary(cursor + scheme.len())
                && line[cursor..cursor + scheme.len()].eq_ignore_ascii_case(scheme)
            {
                cursor += scheme.len();
                cursor = skip_spaces(bytes, cursor);
                break;
            }
        }
        let Some((value_start, value_end, quoted)) = value_span(line, cursor) else {
            out.push_str(&line[idx..cursor]);
            idx = cursor;
            continue;
        };
        let value = &line[value_start..value_end];
        out.push_str(&line[idx..value_start]);
        if is_credential_value(value, quoted, code_line) {
            out.push_str(SECRET_PLACEHOLDER);
            found.push(DetectedSecret {
                value: value.to_string(),
                key,
            });
        } else {
            out.push_str(value);
        }
        idx = value_end;
    }
    out
}

fn skip_spaces(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    i
}

/// The next sensitive key at or after `from`, as a byte range. Matched on
/// identifier boundaries so `passwordField` is not a `password`.
fn next_sensitive_key(line: &str, from: usize) -> Option<(usize, usize)> {
    const KEYS: &[&str] = &[
        "api_key",
        "api-key",
        "apikey",
        "access_key",
        "access-key",
        "access_token",
        "access-token",
        "secret_key",
        "secret-key",
        "client_secret",
        "client-secret",
        "refresh_token",
        "refresh-token",
        "private_key",
        "private-key",
        "signing_key",
        "signing-key",
        "auth_token",
        "auth-token",
        "authorization",
        "passphrase",
        "password",
        "passwd",
        "secret",
        "token",
        "credential",
        "pwd",
    ];
    let lower = line.to_ascii_lowercase();
    let mut best: Option<(usize, usize)> = None;
    for key in KEYS {
        let mut search = from;
        while let Some(rel) = lower[search..].find(key) {
            let start = search + rel;
            let end = start + key.len();
            if is_identifier_boundary(&lower, start, end) {
                if best.is_none_or(|(b, _)| start < b) {
                    best = Some((start, end));
                }
                break;
            }
            search = start + 1;
            if search >= lower.len() {
                break;
            }
        }
    }
    best
}

/// A key match must not be part of a longer identifier — but compound
/// credential names are the norm, not the exception: `API_PASSWORD`,
/// `SERVICE_PASSWORD` and `--api-key` are all real keys whose sensitive word
/// is prefixed. So a separator BEFORE the word is allowed, while a
/// continuation AFTER it still disqualifies (`passwordField`,
/// `my_token_count` are ordinary identifiers, not credentials).
fn is_identifier_boundary(lower: &str, start: usize, end: usize) -> bool {
    let before_ok = lower[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_ascii_alphanumeric());
    let after_ok = lower[end..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    before_ok && after_ok
}

/// The value after a separator: `(start, end, quoted)`.
fn value_span(line: &str, from: usize) -> Option<(usize, usize, bool)> {
    let bytes = line.as_bytes();
    match bytes.get(from) {
        None | Some(b'\n') | Some(b'\r') => None,
        Some(&q @ (b'"' | b'\'')) => {
            let start = from + 1;
            let mut i = start;
            while i < line.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == q {
                    return Some((start, i, true));
                }
                i += 1;
            }
            None
        }
        _ => {
            let end = line[from..]
                .find(|c: char| c.is_whitespace() || c == ',' || c == '}' || c == ';')
                .map(|rel| from + rel)
                .unwrap_or(line.len());
            (end > from).then_some((from, end, false))
        }
    }
}

/// Does this value look like a credential rather than code or a placeholder?
fn is_credential_value(value: &str, quoted: bool, code_line: bool) -> bool {
    let trimmed = value.trim();
    if trimmed.len() < MIN_SECRET_LEN {
        return false;
    }
    const NON_SECRETS: &[&str] = &[
        "[redacted]",
        "<secret>",
        "<redacted>",
        "changeme",
        "password",
        "secret",
        "string",
        "boolean",
        "number",
        "undefined",
    ];
    let lower = trimmed.to_ascii_lowercase();
    if NON_SECRETS.contains(&lower.as_str()) {
        return false;
    }
    if is_code_expression(trimmed) {
        return false;
    }
    // On a line that reads as source code, only a quoted literal is credible
    // as a credential — a bare word there is almost always an expression.
    if code_line && !quoted {
        return false;
    }
    true
}

/// Identifier paths (`config.password`), calls (`get_token()`), types
/// (`String`, `Option<String>`), references (`&str`) and env indirection
/// (`${VAR}`, `process.env.X`) are code, not credentials.
fn is_code_expression(value: &str) -> bool {
    if value.starts_with('&')
        || value.starts_with('$')
        || value.starts_with('*')
        || value.contains("::")
        || value.contains('<')
        || value.contains('(')
        || value.contains('{')
    {
        return true;
    }
    let dotted_identifier = value.contains('.')
        && value.len() <= 64
        && value.split('.').all(|seg| {
            !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        && value
            .split('.')
            .any(|seg| seg.chars().next().is_some_and(|c| c.is_ascii_lowercase()));
    if dotted_identifier {
        return true;
    }
    // A bare CamelCase type name (`String`, `SecretString`).
    value.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && value.chars().all(|c| c.is_ascii_alphanumeric())
        && value.chars().any(|c| c.is_ascii_lowercase())
}

/// Heuristic: does this line read as source code declaring or using a field?
/// Used only to demote BARE values; quoted literals are still redacted.
fn looks_like_code(line: &str) -> bool {
    const MARKERS: &[&str] = &[
        "let ",
        "const ",
        "var ",
        "pub ",
        "fn ",
        "func ",
        "def ",
        "class ",
        "struct ",
        "interface ",
        "impl ",
        "return ",
        "import ",
        "public ",
        "private ",
        "static ",
        "->",
        "=>",
        "::",
        "//",
        "/*",
        "#[",
    ];
    MARKERS.iter().any(|m| line.contains(m))
}

// ── Layer B: session-scoped registry of concrete secret values ─────────────

/// Values a session has already had redacted on the way to the model, so the
/// same plaintext can be scrubbed if it ever reappears in durable text (for
/// example a secret the USER pasted into a goal and the model then repeated).
///
/// Deliberately narrow: exact-value matching only, session-scoped, bounded,
/// never logged and never persisted.
fn registry() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remember `secrets` for `session`. Values too short to match safely are
/// dropped: scrubbing a common token out of every later payload would corrupt
/// far more than it protects.
pub fn register_session_secrets(session: &str, secrets: &[DetectedSecret]) {
    if session.is_empty() || secrets.is_empty() {
        return;
    }
    let mut map = registry().lock().unwrap_or_else(|e| e.into_inner());
    let entry = map.entry(session.to_string()).or_default();
    for secret in secrets {
        let value = secret.value.trim();
        if value.len() < MIN_SECRET_LEN || entry.len() >= MAX_REGISTERED_PER_SESSION {
            continue;
        }
        if !entry.iter().any(|v| v == value) {
            entry.push(value.to_string());
        }
    }
}

/// Replace any registered secret of `session` appearing verbatim in `text`.
/// Returns `text` untouched when the session has no registered values.
pub fn scrub_registered_secrets(session: &str, text: &str) -> String {
    if session.is_empty() || text.is_empty() {
        return text.to_string();
    }
    let map = registry().lock().unwrap_or_else(|e| e.into_inner());
    let Some(values) = map.get(session) else {
        return text.to_string();
    };
    let mut out = text.to_string();
    for value in values {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), SECRET_PLACEHOLDER);
        }
    }
    out
}

/// Drop a session's registered values. Called when the session ends.
pub fn clear_session_secrets(session: &str) {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session);
}

/// Test/diagnostic helper: how many values are registered for a session.
/// Returns a COUNT, never the values themselves.
pub fn registered_secret_count(session: &str) -> usize {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session)
        .map_or(0, Vec::len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sanitize(text: &str) -> String {
        sanitize_model_visible(text).0
    }

    /// POSITIVE — the value positions credentials actually arrive in. The KEY
    /// must survive in every case: the agent has to keep understanding the
    /// shape of the configuration it is working on.
    #[test]
    fn detects_secret_values_in_every_supported_position() {
        let cases: &[(&str, &str)] = &[
            // .env
            ("API_PASSWORD=hunter2-abc", "API_PASSWORD"),
            ("TOKEN=abc123def", "TOKEN"),
            ("SECRET_KEY=s3cret-value", "SECRET_KEY"),
            // export
            ("export API_TOKEN=abc123def", "API_TOKEN"),
            // JSON
            (r#"{"password": "hunter2-abc"}"#, "password"),
            (r#"  "api_key": "abcdef123456","#, "api_key"),
            // YAML
            ("password: hunter2-abc", "password"),
            ("  client_secret: shhh-very-secret", "client_secret"),
            // CLI flags
            ("--password hunter2-abc", "password"),
            ("--token=abc123def", "token"),
            // Authorization headers
            ("Authorization: Bearer tok-abcdefghij", "Authorization"),
            ("authorization: Basic YWxpY2U6cHc=", "authorization"),
            // connection-string style
            ("password=hunter2-abc;host=localhost", "password"),
            ("pwd=hunter2-abc", "pwd"),
        ];
        for (input, key) in cases {
            let out = sanitize(input);
            assert!(
                out.contains(SECRET_PLACEHOLDER),
                "no redaction for {input:?} -> {out:?}"
            );
            assert!(
                out.to_lowercase().contains(&key.to_lowercase()),
                "key {key:?} must survive in {out:?}"
            );
        }
    }

    /// URL userinfo — the credential sits in a position no key/value rule
    /// would find, and the rest of the URL must stay readable.
    #[test]
    fn redacts_url_userinfo_without_destroying_the_url() {
        let out = sanitize("DATABASE_URL=postgres://alice:hunter2-abc@localhost/db");
        assert!(out.contains("postgres://alice:"), "{out}");
        assert!(out.contains("@localhost/db"), "{out}");
        assert!(!out.contains("hunter2-abc"), "{out}");
    }

    /// NEGATIVE — ordinary source code that merely mentions credential words
    /// must come back byte-identical. This is the regression that makes
    /// moving the old scrubber to the model boundary unacceptable.
    #[test]
    fn never_rewrites_ordinary_source_code() {
        let cases = [
            "let password = config.password;",
            "pub password: String,",
            "fn validate_token(token: &str) -> bool {",
            r#"const SECRET_KEY_NAME: &str = "API_KEY";"#,
            "interface Credentials { password: string; }",
            "if user.password.is_empty() { return Err(e); }",
            "please reset your password to continue",
            r#"const passwordField = "password";"#,
            "type Token = String;",
            "// TODO: validate api_key length",
            "self.token = None;",
            "props.token ?? fallback",
        ];
        for input in cases {
            assert_eq!(sanitize(input), input, "code was rewritten: {input:?}");
        }
    }

    /// STRUCTURE PRESERVATION — a config object keeps every key and every
    /// non-secret value, so the agent can still reason about it.
    #[test]
    fn preserves_surrounding_structure() {
        let input = r#"{"username": "alice", "password": "hunter2-abc", "endpoint": "localhost"}"#;
        let out = sanitize(input);
        assert!(out.contains(r#""username": "alice""#), "{out}");
        assert!(out.contains(r#""endpoint": "localhost""#), "{out}");
        assert!(out.contains(r#""password": "[REDACTED]""#), "{out}");
        assert!(!out.contains("hunter2-abc"), "{out}");
    }

    /// A whole .env file: every credential value gone, every key and every
    /// non-credential value intact.
    #[test]
    fn sanitizes_a_dotenv_file_without_blanking_it() {
        let input =
            "SERVICE_USER=test-user\nSERVICE_PASSWORD=hunter2-abc\nENDPOINT=localhost:8080\n";
        let (out, found) = sanitize_model_visible(input);
        assert!(out.contains("SERVICE_USER=test-user"), "{out}");
        assert!(out.contains("ENDPOINT=localhost:8080"), "{out}");
        assert!(out.contains("SERVICE_PASSWORD=[REDACTED]"), "{out}");
        assert!(!out.contains("hunter2-abc"), "{out}");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].value, "hunter2-abc");
        assert_eq!(found[0].key, "SERVICE_PASSWORD");
    }

    /// SMALL VALUE — short or placeholder values are not credentials, and
    /// blanking them would corrupt ordinary text.
    #[test]
    fn leaves_short_and_placeholder_values_alone() {
        for input in [
            "password=1",
            "password: true",
            "token=abc",
            "password: [REDACTED]",
            "password: changeme",
            "TOKEN=",
        ] {
            assert_eq!(sanitize(input), input, "over-redacted: {input:?}");
        }
    }

    /// Multi-line output keeps its line structure.
    #[test]
    fn multiline_output_keeps_its_shape() {
        let input = "line one\nAPI_PASSWORD=hunter2-abc\nline three\n";
        let out = sanitize(input);
        assert_eq!(out.lines().count(), 3, "{out}");
        assert!(out.starts_with("line one\n"), "{out}");
        assert!(out.ends_with("line three\n"), "{out}");
    }

    /// Multi-byte text must never panic the sanitizer. A credential
    /// surrounded by Chinese prose slices mid-character unless every index is
    /// guarded — this is the shape that actually occurred.
    #[test]
    fn multibyte_text_is_sanitized_without_panicking() {
        for input in [
            "密码 password: 秘密值abc 之后",
            "配置里 API_PASSWORD=值值值值值值 就是它",
            "authorization: 令牌值令牌值",
            "注释：token= 之后没有值",
        ] {
            let _ = sanitize_model_visible(input);
        }
    }

    /// Self-identifying provider keys carry no key position, so they need
    /// their own rule. Coverage must not regress from the pre-F6 scrubber.
    #[test]
    fn detects_self_identifying_provider_keys() {
        for input in [
            "sk-abcdefghijklmnop1234",
            "ghp_abcdefghijklmnopqrst",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            let (out, _) = sanitize_model_visible(input);
            assert_ne!(out, input, "provider key not detected: {input}");
        }
    }

    // ── Layer B: session-scoped registry ──────────────────────────────────

    /// The F6 accident itself: a value the harness already identified must
    /// not re-enter durable text just because it was repeated in prose.
    #[test]
    fn registered_secrets_are_scrubbed_from_later_prose() {
        let session = "sess-echo-test";
        clear_session_secrets(session);
        let (_, found) = sanitize_model_visible("API_PASSWORD=hunter2-unique-echo");
        register_session_secrets(session, &found);

        let prose = "The API password is hunter2-unique-echo, note it down.";
        let scrubbed = scrub_registered_secrets(session, prose);
        assert!(!scrubbed.contains("hunter2-unique-echo"), "{scrubbed}");
        assert!(scrubbed.contains("The API password is"), "{scrubbed}");
        clear_session_secrets(session);
    }

    /// Sessions must not leak into each other.
    #[test]
    fn registry_is_isolated_per_session() {
        let (a, b) = ("sess-iso-a", "sess-iso-b");
        clear_session_secrets(a);
        clear_session_secrets(b);
        let (_, found) = sanitize_model_visible("PASSWORD=alpha-secret-value");
        register_session_secrets(a, &found);

        let text = "mentions alpha-secret-value verbatim";
        assert!(!scrub_registered_secrets(a, text).contains("alpha-secret-value"));
        assert_eq!(
            scrub_registered_secrets(b, text),
            text,
            "session B must be unaffected"
        );
        clear_session_secrets(a);
        clear_session_secrets(b);
    }

    /// Short values are never registered — scrubbing them everywhere would
    /// corrupt unrelated text.
    #[test]
    fn registry_refuses_values_too_short_to_match_safely() {
        let session = "sess-short";
        clear_session_secrets(session);
        register_session_secrets(
            session,
            &[DetectedSecret {
                value: "abc".into(),
                key: "token".into(),
            }],
        );
        assert_eq!(registered_secret_count(session), 0);
        clear_session_secrets(session);
    }

    #[test]
    fn registry_is_bounded_and_deduplicated() {
        let session = "sess-bound";
        clear_session_secrets(session);
        let dup = DetectedSecret {
            value: "hunter2-abc".into(),
            key: "password".into(),
        };
        register_session_secrets(session, &[dup.clone(), dup.clone(), dup]);
        assert_eq!(registered_secret_count(session), 1, "duplicates collapse");

        let many: Vec<DetectedSecret> = (0..(MAX_REGISTERED_PER_SESSION + 50))
            .map(|i| DetectedSecret {
                value: format!("secret-value-{i:04}"),
                key: "token".into(),
            })
            .collect();
        register_session_secrets(session, &many);
        assert_eq!(registered_secret_count(session), MAX_REGISTERED_PER_SESSION);
        clear_session_secrets(session);
        assert_eq!(registered_secret_count(session), 0);
    }

    /// A session with nothing registered must not pay for the lookup, and
    /// must not alter the text.
    #[test]
    fn scrubbing_is_a_no_op_without_registered_secrets() {
        let text = "nothing sensitive here";
        assert_eq!(scrub_registered_secrets("sess-empty", text), text);
        assert_eq!(scrub_registered_secrets("", text), text);
    }
}
