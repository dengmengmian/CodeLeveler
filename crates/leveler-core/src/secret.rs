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

/// A PEM block or a very long free-form value is still redacted where it was
/// found; remembering the whole block for later prose matching is not useful
/// and would keep multi-kilobyte secrets in memory.
const MAX_REGISTERED_VALUE_LEN: usize = 4096;

/// One detected secret: where it sat and what the value was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedSecret {
    /// The concrete value. Never logged, never persisted.
    pub value: String,
    /// The key or context it was found under, e.g. `API_PASSWORD`. Safe to
    /// log — it is a name, not a credential.
    pub key: String,
}

// ── Structured detection for text a person typed ───────────────────────────
//
// The sanitizer above answers "what must not reach the model". This detector
// answers a different question: "what might be a credential in the user's own
// words, and how sure are we?" It never rewrites anything. The caller decides
// whether to capture, ask, or send unchanged, and that decision is a user
// decision — detection only describes risk.

/// How strongly the evidence says a span is a real credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretLevel {
    /// A self-identifying format, or several independent signals agreeing.
    /// This value must not be sent to a model without the person confirming.
    HighConfidence,
    /// Credential-shaped, but ordinary code, a hash, or a placeholder cannot
    /// be ruled out. The user decides.
    Ambiguous,
}

impl SecretLevel {
    /// Stable rank used to resolve overlapping findings.
    fn rank(self) -> u8 {
        match self {
            Self::HighConfidence => 2,
            Self::Ambiguous => 1,
        }
    }
}

/// What kind of credential position the finding occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    /// A complete PEM private-key block.
    PemPrivateKey,
    /// A value with a provider-issued, self-identifying prefix (`sk-…`,
    /// `ghp_…`, `AKIA…`).
    ProviderKey,
    /// A JWT (`header.payload.signature`).
    JsonWebToken,
    /// Credentials embedded in a URL authority (`scheme://user:pass@host`).
    UrlCredentials,
    /// The value of a credential-named key (`PASSWORD=…`,
    /// `"api_key": "…"`, `--token …`).
    KeyValue,
    /// An opaque token next to credential words in prose.
    ProseCredential,
}

/// One independent reason the detector classified a span the way it did.
/// Diagnostics and tests read these; the UI shows a localized summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretEvidence {
    /// `-----BEGIN … PRIVATE KEY-----`
    PrivateKeyMarker,
    /// A known provider token prefix.
    KnownPrefix,
    /// The value sits after a credential-named key.
    CredentialKeyName,
    /// The value follows a credential scheme word (`Bearer`, `Basic`).
    CredentialScheme,
    /// The value is the password field of a URL authority.
    UrlUserInfo,
    /// Long value with no whitespace, mixing letters and digits.
    OpaqueValue,
    /// The value has high Shannon entropy relative to its length.
    HighEntropy,
    /// Credential words appear next to a standalone token in natural prose.
    ProseCue,
}

/// A credential candidate in the user's text.
///
/// `start..end` is a byte range into the ORIGINAL text and always lands on
/// UTF-8 character boundaries. Only the span is reported — the value itself is
/// read from the input by the caller that already holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    /// Inclusive byte offset of the credential value.
    pub start: usize,
    /// Exclusive byte offset of the credential value.
    pub end: usize,
    /// How strongly the evidence supports "this is a real credential".
    pub level: SecretLevel,
    /// The credential position this span occupies.
    pub kind: SecretKind,
    /// The field or header name it was found under, when there is one. Safe
    /// to log and show: it is a name, never the value.
    pub key: Option<String>,
    /// The independent signals behind this classification.
    pub evidence: Vec<SecretEvidence>,
}

impl SecretFinding {
    /// The value this finding covers.
    pub fn value<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start..self.end]
    }
}

/// A masked description of a credential value, safe for a terminal, a log or
/// a UI prompt. Reveals only the length — never a prefix that could identify
/// the credential itself.
pub fn masked_secret(value: &str) -> String {
    let chars = value.chars().count();
    format!("•••••• ({chars} chars)")
}

/// The transcript token for a captured reference: `[secret:sec_…]`.
///
/// One spelling, defined here, because the model reads it in a message, the
/// client renders it, and the broker tool parses it back.
pub fn secret_reference_token(reference: &str) -> String {
    format!("[secret:{reference}]")
}

/// The reference inside a transcript token, or `None` when the token is not a
/// well-formed reference. This validates the shape only; the vault decides
/// whether the reference resolves.
pub fn parse_secret_reference_token(token: &str) -> Option<&str> {
    let inner = token.strip_prefix("[secret:")?.strip_suffix(']')?;
    let hex = inner.strip_prefix("sec_")?;
    (hex.len() == 32
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(inner)
}

/// Validate and normalize a workspace-relative dotenv target.
///
/// The broker writes dotenv files only. This is the one shape check shared by
/// the host broker and the capture-time use scope: a relative path whose file
/// name is `.env` or `.env.<suffix>`. Absolute paths, parent traversal, drive
/// letters and non-dotenv names are not targets.
pub fn normalize_dotenv_target(target: &str) -> Option<String> {
    let trimmed = target.trim();
    if trimmed.is_empty()
        || trimmed.len() > 1024
        || trimmed.contains(['\0', '\n', '\r'])
        || trimmed.starts_with('/')
        || trimmed.starts_with('\\')
        || trimmed.contains(':')
    {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in trimmed.split(['/', '\\']) {
        match part {
            "" | "." => continue,
            ".." => return None,
            other => parts.push(other),
        }
    }
    let file = *parts.last()?;
    if file != ".env" && !file.starts_with(".env.") {
        return None;
    }
    Some(parts.join("/"))
}

/// Bytes that may belong to one path-shaped token. Only ASCII: any non-ASCII
/// character is prose adjacent to the path, not part of it.
fn is_target_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'/' | b'\\')
}

/// Every dotenv target the user's own text names.
///
/// This is the authorization source for a captured credential's write target:
/// a message that says "configure this in `.env`" lets the broker write that
/// reference to `.env`, and nowhere else. It deliberately does not parse
/// natural language — it extracts path-shaped tokens whose file name is a
/// dotenv file. A message that names no target authorizes none.
pub fn dotenv_targets_in(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut targets = std::collections::BTreeSet::new();
    let mut cursor = 0usize;
    while let Some(relative) = text[cursor..].find(".env") {
        let hit = cursor + relative;
        let mut start = hit;
        while start > 0 && is_target_path_byte(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = hit + 4;
        while end < bytes.len() && is_target_path_byte(bytes[end]) {
            end += 1;
        }
        if let Some(target) = normalize_dotenv_target(&text[start..end]) {
            targets.insert(target);
        }
        if targets.len() >= 16 {
            break;
        }
        cursor = end.max(hit + 1);
    }
    targets.into_iter().collect()
}

/// Whether `name` can be a dotenv key.
fn is_dotenv_field_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Every dotenv field the user's own text assigns (`NAME=`).
///
/// Only explicit assignment syntax counts. A label in prose ("password: …")
/// is not reliably the environment variable name the user wants, so it is not
/// turned into a field authorization.
pub fn dotenv_fields_in(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut fields = std::collections::BTreeSet::new();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'=' {
            continue;
        }
        // `==`, `=>`, `=<` and `=~` are comparisons or arrows, not assignments.
        if matches!(
            bytes.get(index + 1),
            Some(b'=') | Some(b'>') | Some(b'<') | Some(b'~')
        ) {
            continue;
        }
        let mut end = index;
        while end > 0 && matches!(bytes[end - 1], b' ' | b'\t') {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            start -= 1;
        }
        if start == end {
            continue;
        }
        // A `=` directly before the name means this is another assignment's
        // value (`A=B=C`), not a field of its own.
        if start > 0 && matches!(bytes[start - 1], b'=' | b'>' | b'<' | b'!') {
            continue;
        }
        let name = &text[start..end];
        if is_dotenv_field_name(name) {
            fields.insert(name.to_string());
            if fields.len() >= 16 {
                break;
            }
        }
    }
    fields.into_iter().collect()
}

/// Detect credential candidates in text a person typed.
///
/// Deterministic and local: no network, no model, no entropy-only guessing.
/// Findings are non-overlapping and sorted by position. Empty text, ordinary
/// code, templates and environment references return no findings.
pub fn detect_secrets(text: &str) -> Vec<SecretFinding> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    // Our own custody tokens are not credentials. Blank them with spaces of
    // the same byte length so every later offset still addresses the original
    // text; the vault, not the detector, decides whether they resolve.
    let text = &mask_reference_tokens(text);
    let mut findings = Vec::new();
    let mut occupied: Vec<(usize, usize)> = Vec::new();
    scan_private_key_blocks(text, &mut findings, &mut occupied);
    scan_block_scalars(text, &mut findings);

    let mut line_start = 0usize;
    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        if !occupied
            .iter()
            .any(|(start, end)| line_start < *end && line_end > *start)
            && !line.trim().is_empty()
        {
            let mut line_findings = known_token_findings(line, line_start);
            line_findings.extend(assignment_findings(line, line_start));
            line_findings.extend(url_credential_findings(line, line_start));
            line_findings.extend(prose_credential_findings(line, line_start));
            findings.extend(line_findings);
        }
        line_start = line_end;
    }
    resolve_findings(findings)
}

/// Blank every `[secret:sec_…]` token, preserving byte offsets. Tokens are
/// ASCII, so the masked copy is valid UTF-8 and the same length.
fn mask_reference_tokens(text: &str) -> String {
    let mut out = text.to_string();
    let mut search = 0usize;
    while let Some(rel) = out[search..].find("[secret:") {
        let start = search + rel;
        let Some(close) = out[start..].find(']') else {
            break;
        };
        let end = start + close + 1;
        if parse_secret_reference_token(&out[start..end]).is_some() {
            out.replace_range(start..end, &" ".repeat(end - start));
        }
        search = end;
        if search >= out.len() {
            break;
        }
    }
    out
}

/// Byte ranges of complete `[secret:sec_…]` tokens on one line.
fn reference_token_ranges(line: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = line[search..].find("[secret:") {
        let start = search + rel;
        let Some(close) = line[start..].find(']') else {
            break;
        };
        let end = start + close + 1;
        if parse_secret_reference_token(&line[start..end]).is_some() {
            ranges.push((start, end));
        }
        search = end;
        if search >= line.len() {
            break;
        }
    }
    ranges
}

fn kind_rank(kind: SecretKind) -> u8 {
    match kind {
        SecretKind::ProviderKey | SecretKind::JsonWebToken | SecretKind::PemPrivateKey => 4,
        SecretKind::UrlCredentials => 3,
        SecretKind::KeyValue => 2,
        SecretKind::ProseCredential => 1,
    }
}

/// Keep one finding per credential position. Overlaps are resolved by
/// confidence, then by how specifically the credential position is named —
/// `Authorization: Bearer sk-…` must yield the token span, not the whole
/// header value, so a caller can substitute the value without destroying the
/// syntax around it.
fn resolve_findings(mut findings: Vec<SecretFinding>) -> Vec<SecretFinding> {
    findings.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(a.end.cmp(&b.end))
            .then(b.level.rank().cmp(&a.level.rank()))
    });
    let mut out: Vec<SecretFinding> = Vec::new();
    for candidate in findings {
        if let Some(last) = out.last_mut() {
            if candidate.start == last.start && candidate.end == last.end {
                if candidate.level.rank() > last.level.rank()
                    || (candidate.level.rank() == last.level.rank()
                        && kind_rank(candidate.kind) > kind_rank(last.kind))
                {
                    *last = candidate;
                }
                continue;
            }
            if candidate.start < last.end {
                let replace = candidate.level.rank() > last.level.rank()
                    || (candidate.level.rank() == last.level.rank()
                        && (kind_rank(candidate.kind) > kind_rank(last.kind)
                            || (kind_rank(candidate.kind) == kind_rank(last.kind)
                                && (candidate.end - candidate.start) < (last.end - last.start))));
                if replace {
                    *last = candidate;
                }
                continue;
            }
        }
        out.push(candidate);
    }
    out
}

// ── Multi-line positions ───────────────────────────────────────────────────

/// Complete private-key blocks. A block that starts and never ends is
/// reported through the end of the text: fail closed, exactly like the
/// sanitizer's rule.
fn scan_private_key_blocks(
    text: &str,
    findings: &mut Vec<SecretFinding>,
    occupied: &mut Vec<(usize, usize)>,
) {
    let mut line_start = 0usize;
    let mut begin: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        if begin.is_none() && is_private_key_marker(line, true) {
            let marker = line.to_ascii_lowercase().find("-----begin ");
            begin = Some(line_start + marker.unwrap_or(0));
        } else if let Some(start) = begin
            && is_private_key_marker(line, false)
        {
            let content = line.trim_end_matches(['\n', '\r']);
            let end = line_start + content.len();
            findings.push(SecretFinding {
                start,
                end,
                level: SecretLevel::HighConfidence,
                kind: SecretKind::PemPrivateKey,
                key: None,
                evidence: vec![SecretEvidence::PrivateKeyMarker],
            });
            occupied.push((start, end));
            begin = None;
        }
        line_start += line.len();
    }
    if let Some(start) = begin {
        let end = text.trim_end_matches(['\n', '\r']).len();
        findings.push(SecretFinding {
            start,
            end,
            level: SecretLevel::HighConfidence,
            kind: SecretKind::PemPrivateKey,
            key: None,
            evidence: vec![SecretEvidence::PrivateKeyMarker],
        });
        occupied.push((start, end));
    }
}

/// `password: |` followed by indented body lines. One credential, one span.
fn scan_block_scalars(text: &str, findings: &mut Vec<SecretFinding>) {
    let mut offset = 0usize;
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let line_offset = offset;
        offset += line.len();
        let Some(indent) = block_scalar_indent(line) else {
            continue;
        };
        let key = next_sensitive_key(line, 0).map(|(start, end)| line[start..end].to_string());
        let mut start: Option<usize> = None;
        let mut end = line_offset + line.trim_end_matches(['\n', '\r']).len();
        let mut body_offset = offset;
        let mut consumed = 0usize;
        for body in lines.clone() {
            let trimmed = body.trim_start_matches([' ', '\t']);
            let leading = body.len() - trimmed.len();
            if trimmed.trim_end_matches(['\n', '\r']).is_empty() {
                body_offset += body.len();
                consumed += 1;
                continue;
            }
            if leading < indent {
                break;
            }
            let content_start = body_offset + leading;
            let content_end = body_offset + body.trim_end_matches(['\n', '\r']).len();
            start.get_or_insert(content_start);
            end = content_end;
            body_offset += body.len();
            consumed += 1;
        }
        if let Some(start) = start {
            let value = &text[start..end];
            if !value.trim().is_empty() && !is_template_value(value.trim()) {
                findings.push(SecretFinding {
                    start,
                    end,
                    level: SecretLevel::HighConfidence,
                    kind: SecretKind::KeyValue,
                    key,
                    evidence: vec![
                        SecretEvidence::CredentialKeyName,
                        SecretEvidence::OpaqueValue,
                    ],
                });
            }
        }
        for _ in 0..consumed {
            let _ = lines.next();
        }
    }
}

// ── Known provider formats ────────────────────────────────────────────────

fn boundary_before(line: &str, start: usize) -> bool {
    line[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
}

fn boundary_after(line: &str, end: usize) -> bool {
    line[end..]
        .chars()
        .next()
        .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
}

fn take_while_byte(line: &str, mut i: usize, f: impl Fn(u8) -> bool) -> usize {
    let bytes = line.as_bytes();
    while let Some(&b) = bytes.get(i) {
        if !f(b) {
            break;
        }
        i += 1;
    }
    i
}

fn token_body(line: &str, start: usize) -> usize {
    take_while_byte(line, start, |b| {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
    })
}

fn known_token_findings(line: &str, offset: usize) -> Vec<SecretFinding> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < line.len() {
        let rest = &line[i..];
        let mut matched: Option<(usize, SecretKind)> = None;
        if rest.starts_with("sk-") {
            let end = token_body(line, i + 3);
            if boundary_before(line, i) && boundary_after(line, end) && end - (i + 3) >= 16 {
                matched = Some((end, SecretKind::ProviderKey));
            }
        } else if rest.starts_with("sk_live_") || rest.starts_with("sk_test_") {
            let prefix = 8;
            let end = token_body(line, i + prefix);
            if boundary_before(line, i) && boundary_after(line, end) && end - (i + prefix) >= 12 {
                matched = Some((end, SecretKind::ProviderKey));
            }
        } else if ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
            .iter()
            .any(|prefix| rest.starts_with(prefix))
        {
            let end = token_body(line, i + 4);
            if boundary_before(line, i) && boundary_after(line, end) && end - (i + 4) >= 16 {
                matched = Some((end, SecretKind::ProviderKey));
            }
        } else if rest.starts_with("github_pat_") {
            let end = token_body(line, i + "github_pat_".len());
            if boundary_before(line, i)
                && boundary_after(line, end)
                && end - (i + "github_pat_".len()) >= 16
            {
                matched = Some((end, SecretKind::ProviderKey));
            }
        } else if rest.starts_with("AKIA") {
            let end = i + 20;
            if end <= line.len()
                && bytes[i + 4..end]
                    .iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                && boundary_before(line, i)
                && boundary_after(line, end)
            {
                matched = Some((end, SecretKind::ProviderKey));
            }
        } else if ["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"]
            .iter()
            .any(|prefix| rest.starts_with(prefix))
        {
            let end = token_body(line, i + 5);
            if boundary_before(line, i) && boundary_after(line, end) && end - (i + 5) >= 10 {
                matched = Some((end, SecretKind::ProviderKey));
            }
        } else if rest.starts_with("AIza") {
            let end = token_body(line, i + 4);
            let total = end - i;
            if boundary_before(line, i) && boundary_after(line, end) && (35..=45).contains(&total) {
                matched = Some((end, SecretKind::ProviderKey));
            }
        } else if rest.starts_with("eyJ")
            && let Some(end) = jwt_end(line, i)
        {
            matched = Some((end, SecretKind::JsonWebToken));
        }
        if let Some((end, kind)) = matched {
            out.push(SecretFinding {
                start: offset + i,
                end: offset + end,
                level: SecretLevel::HighConfidence,
                kind,
                key: None,
                evidence: vec![SecretEvidence::KnownPrefix],
            });
            i = end;
            continue;
        }
        i += line[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

/// The end of a JWT that starts at `start`, or `None` when the run is not a
/// three-part JWT. Two dots and a plausible header are the format proof; the
/// signature is not decoded and the token is never sent anywhere.
fn jwt_end(line: &str, start: usize) -> Option<usize> {
    let end = take_while_byte(line, start, |b| {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
    });
    if end.saturating_sub(start) < 40 || !boundary_after(line, end) {
        return None;
    }
    let token = &line[start..end];
    let parts: Vec<&str> = token.split('.').collect();
    (parts.len() == 3
        && parts.iter().all(|part| !part.is_empty())
        && parts[0].starts_with("eyJ")
        && parts.iter().all(|part| {
            part.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        }))
    .then_some(end)
}

// ── Key/value positions ───────────────────────────────────────────────────

fn assignment_findings(line: &str, offset: usize) -> Vec<SecretFinding> {
    let code_line = looks_like_code(line);
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut idx = 0usize;
    let mut guard = 0usize;
    while idx < line.len() && guard < 64 {
        guard += 1;
        let Some((key_start, key_end)) = next_sensitive_key(line, idx) else {
            break;
        };
        let name_start = line[..key_start]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
            .last()
            .map_or(key_start, |(i, _)| i);
        let key = line[name_start..key_end].to_string();
        let mut cursor = key_end;
        if matches!(bytes.get(cursor), Some(b'"') | Some(b'\'') | Some(b'`')) {
            cursor += 1;
        }
        cursor = skip_spaces(bytes, cursor);
        let before_key = &line[..key_start];
        let flag_style = before_key.ends_with("--")
            || (before_key.ends_with('-')
                && !before_key[..before_key.len() - 1]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric()));
        let sep = bytes.get(cursor).copied();
        let has_sep = matches!(sep, Some(b'=') | Some(b':'));
        if has_sep {
            if matches!(bytes.get(cursor + 1), Some(b'=') | Some(b'>') | Some(b':')) {
                idx = key_end;
                continue;
            }
            cursor += 1;
        } else if !(flag_style && cursor > key_end) {
            idx = key_end;
            continue;
        }
        cursor = skip_spaces(bytes, cursor);
        let mut credential_scheme = false;
        for scheme in ["bearer ", "basic ", "token "] {
            if line.len() >= cursor + scheme.len()
                && line.is_char_boundary(cursor)
                && line.is_char_boundary(cursor + scheme.len())
                && line[cursor..cursor + scheme.len()].eq_ignore_ascii_case(scheme)
            {
                cursor += scheme.len();
                cursor = skip_spaces(bytes, cursor);
                credential_scheme = true;
                break;
            }
        }
        let Some((value_start, value_end, quoted)) = value_span(line, cursor) else {
            if cursor <= idx {
                break;
            }
            idx = cursor;
            continue;
        };
        let value = &line[value_start..value_end];
        if let Some(level) = classify_assignment(&key, value, quoted, code_line, credential_scheme)
        {
            let mut evidence = vec![SecretEvidence::CredentialKeyName];
            if credential_scheme {
                evidence.push(SecretEvidence::CredentialScheme);
            }
            if is_opaque(value) {
                evidence.push(SecretEvidence::OpaqueValue);
            }
            out.push(SecretFinding {
                start: offset + value_start,
                end: offset + value_end,
                level,
                kind: SecretKind::KeyValue,
                key: Some(key),
                evidence,
            });
        }
        idx = value_end.max(idx + 1);
    }
    out
}

fn classify_assignment(
    key: &str,
    value: &str,
    quoted: bool,
    code_line: bool,
    credential_scheme: bool,
) -> Option<SecretLevel> {
    let value = value.trim();
    if value.len() < MIN_SECRET_LEN
        || is_template_value(value)
        || is_placeholder_value(value)
        || is_hash_or_uuid(value)
    {
        return None;
    }
    if is_bare_key_name(key) && !is_opaque_key_value(value) {
        return None;
    }
    let lower_key = key.to_ascii_lowercase();
    let header = lower_key == "authorization" || lower_key.ends_with("authorization");
    if header || credential_scheme {
        return (is_opaque(value) && value.len() >= 8).then_some(SecretLevel::HighConfidence);
    }
    // A code expression is not a credential literal. Quoted literals stay
    // eligible because the source really contains that value.
    if !quoted && is_code_expression(value) {
        return None;
    }
    if code_line && !quoted && is_bare_identifier(value) {
        return None;
    }
    if is_non_secret_prose(value) {
        return None;
    }
    let entropy = shannon_entropy(value);
    let opaque = is_opaque(value);
    if opaque
        && value.len() >= 12
        && value.chars().any(|c| c.is_ascii_alphabetic())
        && value.chars().any(|c| c.is_ascii_digit())
        && entropy >= 3.0
    {
        return Some(SecretLevel::HighConfidence);
    }
    if opaque && value.len() >= MIN_SECRET_LEN {
        return Some(SecretLevel::Ambiguous);
    }
    if !opaque && value.len() >= 12 && value.split_whitespace().count() >= 2 && entropy >= 3.2 {
        return Some(SecretLevel::Ambiguous);
    }
    None
}

// ── URLs ──────────────────────────────────────────────────────────────────

fn url_credential_findings(line: &str, offset: usize) -> Vec<SecretFinding> {
    let mut out = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = line[search..].find("://") {
        let scheme_end = search + rel + 3;
        let tail = &line[scheme_end..];
        let authority_end = tail
            .find(|c: char| {
                c == '/' || c == '?' || c == '#' || c.is_whitespace() || c == '"' || c == '\''
            })
            .unwrap_or(tail.len());
        let authority = &tail[..authority_end];
        if let (Some(at), Some(colon)) = (authority.find('@'), authority.find(':'))
            && colon < at
        {
            let value_start = scheme_end + colon + 1;
            let value_end = scheme_end + at;
            let value = &line[value_start..value_end];
            if value.len() >= MIN_SECRET_LEN && !is_template_value(value) {
                out.push(SecretFinding {
                    start: offset + value_start,
                    end: offset + value_end,
                    level: SecretLevel::HighConfidence,
                    kind: SecretKind::UrlCredentials,
                    key: None,
                    evidence: vec![SecretEvidence::UrlUserInfo, SecretEvidence::OpaqueValue],
                });
            }
        }
        search = scheme_end + authority_end;
        if search >= line.len() {
            break;
        }
    }
    out
}

// ── Prose mentions ────────────────────────────────────────────────────────

const PROSE_CUES: &[&str] = &[
    "api key",
    "api_key",
    "apikey",
    "token",
    "password",
    "passwd",
    "secret",
    "credential",
    "密钥",
    "令牌",
    "密码",
    "凭据",
    "这个值",
    "该值",
    "配置进去",
    "配置到",
    "填入",
    "填到",
    "设置为",
    "this value",
];

fn prose_credential_findings(line: &str, offset: usize) -> Vec<SecretFinding> {
    if looks_like_code(line) || is_comment_line(line) {
        return Vec::new();
    }
    let lower = line.to_ascii_lowercase();
    let mut out = Vec::new();
    for cue in PROSE_CUES {
        let mut search = 0usize;
        while let Some(rel) = lower[search..].find(cue) {
            let after_cue = search + rel + cue.len();
            let window_end = (after_cue + 64).min(line.len());
            let window_start = crate::text::floor_char_boundary(line, after_cue);
            let window_end = crate::text::ceil_char_boundary(line, window_end);
            for (start, end) in ascii_token_spans(&line[window_start..window_end]) {
                let start = window_start + start;
                let end = window_start + end;
                let value = &line[start..end];
                if plausible_unlabelled_credential(value) {
                    out.push(SecretFinding {
                        start: offset + start,
                        end: offset + end,
                        level: SecretLevel::Ambiguous,
                        kind: SecretKind::ProseCredential,
                        key: None,
                        evidence: vec![SecretEvidence::ProseCue, SecretEvidence::OpaqueValue],
                    });
                    break;
                }
            }
            search = after_cue;
            if search >= lower.len() {
                break;
            }
        }
    }
    out
}

fn ascii_token_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphanumeric() {
            let start = i;
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'-'))
            {
                i += 1;
            }
            spans.push((start, i));
        } else {
            i += 1;
        }
    }
    spans
}

fn plausible_unlabelled_credential(value: &str) -> bool {
    if value.len() < 8 || value.len() > 120 {
        return false;
    }
    if is_hash_or_uuid(value) || looks_like_encoded_text(value) {
        return false;
    }
    if value.chars().all(|c| c.is_ascii_digit()) {
        // A long number can be a PIN or password, but it is also a build id.
        return value.len() >= 8;
    }
    let has_mixed = value.chars().any(|c| c.is_ascii_uppercase())
        && value.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = value.chars().any(|c| c.is_ascii_digit());
    let has_separator = value.contains('-') || value.contains('_');
    has_digit && (has_mixed || has_separator || shannon_entropy(value) >= 3.4)
}

// ── Value shape helpers ───────────────────────────────────────────────────

fn is_opaque(value: &str) -> bool {
    !value.chars().any(char::is_whitespace)
}

fn shannon_entropy(value: &str) -> f64 {
    if value.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for byte in value.bytes() {
        counts[byte as usize] += 1;
    }
    let len = value.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let probability = *count as f64 / len;
            -probability * probability.log2()
        })
        .sum()
}

/// Values that occupy a credential position syntactically but are not a
/// credential: environment indirection, templates, documentation
/// placeholders and redaction markers.
fn is_template_value(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    if value.starts_with("${")
        || value.starts_with("${{")
        || value.starts_with("{{")
        || (value.starts_with('<') && value.ends_with('>'))
        || (value.starts_with('%') && value.ends_with('%'))
        || value == "[REDACTED]"
        || value.eq_ignore_ascii_case("[redacted]")
        || value == "***"
        || value == "..."
        || value == "…"
    {
        return true;
    }
    if value.starts_with('$')
        && value[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return true;
    }
    if value.contains("process.env")
        || value.contains("os.getenv")
        || value.contains("os.environ")
        || value.contains("std::env")
        || value.contains("env(")
        || value.contains("${")
    {
        return true;
    }
    let first_word = lower.split(['-', '_', ' ', '.']).next().unwrap_or("");
    matches!(
        first_word,
        "your"
            | "example"
            | "sample"
            | "dummy"
            | "placeholder"
            | "changeme"
            | "replace"
            | "insert"
            | "xxx"
            | "redacted"
            | "testkey"
    ) || matches!(
        lower.as_str(),
        "password"
            | "secret"
            | "token"
            | "string"
            | "boolean"
            | "number"
            | "undefined"
            | "required"
            | "invalid"
            | "optional"
            | "enabled"
            | "disabled"
            | "true"
            | "false"
            | "none"
            | "null"
            | "bearer"
            | "basic"
            | "completed"
            | "pending"
            | "running"
            | "failed"
            | "success"
            | "valid"
    )
}

/// Hashes, commit SHAs and UUIDs are opaque and random-looking, and are
/// ordinary development material. Demote them: a credential that happens to
/// be a bare hex digest is indistinguishable from a hash, so claiming high
/// confidence would be dishonest.
fn is_hash_or_uuid(value: &str) -> bool {
    if !value.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return false;
    }
    let digits = value.chars().filter(|c| c.is_ascii_hexdigit()).count();
    let dashes = value.chars().filter(|c| *c == '-').count();
    if digits == 32 && dashes == 4 && value.len() == 36 {
        return true;
    }
    dashes == 0 && matches!(digits, 32 | 40 | 64 | 128)
}

/// Base64 of printable text: no digits, standard alphabet, padded. This is
/// the shape of encoded logs and fixtures, not of a provider credential.
fn looks_like_encoded_text(value: &str) -> bool {
    value.len() >= 16
        && value.len().is_multiple_of(4)
        && value.ends_with('=')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        && !value.bytes().any(|b| b.is_ascii_digit())
}

/// Sanitize text that is about to become model-visible, returning the text
/// with secret VALUES replaced and the list of values that were found.
///
/// Keys, structure, code, and prose are preserved. Safe to run on arbitrary
/// tool output — source files, shell output, JSON, YAML, .env, logs. Private
/// key blocks and sensitive block scalars are handled before the line pass so
/// a multi-line credential is one classified value, not its first line.
pub fn sanitize_model_visible(text: &str) -> (String, Vec<DetectedSecret>) {
    if text.is_empty() {
        return (String::new(), Vec::new());
    }
    let (text, mut found) = redact_private_key_blocks(text);
    let mut out = String::with_capacity(text.len());
    let mut block_indent: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        if let Some(indent) = block_indent {
            let (source_start, content) = source_line(line);
            let trimmed = content.trim_start_matches([' ', '\t']);
            let leading = content.len() - trimmed.len();
            if trimmed.is_empty() {
                out.push_str(line);
                continue;
            }
            if leading >= indent {
                let newline = if line.ends_with('\n') { "\n" } else { "" };
                let value = trimmed.trim_end_matches(['\n', '\r']).trim();
                if !value.is_empty() && value != SECRET_PLACEHOLDER {
                    found.push(DetectedSecret {
                        value: value.to_string(),
                        key: "block scalar".to_string(),
                    });
                }
                out.push_str(&line[..source_start]);
                out.push_str(&content[..leading]);
                out.push_str(SECRET_PLACEHOLDER);
                out.push_str(newline);
                continue;
            }
            block_indent = None;
        }
        let sanitized = sanitize_line(line, &mut found);
        if let Some(indent) = block_scalar_indent(line) {
            block_indent = Some(indent);
        }
        out.push_str(&sanitized);
    }
    (out, found)
}

/// Replace complete PEM private-key blocks. A block that starts and never ends
/// is redacted through the end of the text: fail closed rather than expose the
/// first line and leave the body readable.
fn redact_private_key_blocks(text: &str) -> (String, Vec<DetectedSecret>) {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut found = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if is_private_key_marker(lines[i], true) {
            let mut end = i;
            while end < lines.len() && !is_private_key_marker(lines[end], false) {
                end += 1;
            }
            let last = end.min(lines.len().saturating_sub(1));
            let values = lines[i..=last].concat();
            found.push(DetectedSecret {
                value: values.trim_end().to_string(),
                key: "PEM private key".to_string(),
            });
            out.push_str(SECRET_PLACEHOLDER);
            if lines[last].ends_with('\n') {
                out.push('\n');
            }
            i = last + 1;
            continue;
        }
        out.push_str(lines[i]);
        i += 1;
    }
    (out, found)
}

fn is_private_key_marker(line: &str, begin: bool) -> bool {
    let lower = line.to_ascii_lowercase();
    let marker = if begin { "-----begin " } else { "-----end " };
    lower.contains(marker) && lower.contains("private key-----")
}

/// Read_file renders each line as a right-aligned number, a tab, then the
/// source text. Block-scalar indentation must be measured against the source
/// text, not the renderer's number prefix.
fn source_line(line: &str) -> (usize, &str) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while matches!(bytes.get(i), Some(b' ')) {
        i += 1;
    }
    let digits = i;
    while matches!(bytes.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    if i > digits && matches!(bytes.get(i), Some(b'\t') | Some(b' ')) {
        return (i + 1, &line[i + 1..]);
    }
    (0, line)
}

/// A YAML/Ansible-style block scalar under a sensitive key:
///   password: |
///     value-on-the-next-lines
/// Returns the minimum indentation that belongs to the block. The header line
/// itself is left intact; its indented body is replaced by the placeholder.
fn block_scalar_indent(line: &str) -> Option<usize> {
    let (_, content) = source_line(line);
    let (_, key_end) = next_sensitive_key(content, 0)?;
    let after = content[key_end..].trim_start();
    let value = after.strip_prefix(':')?.trim();
    if matches!(value, "|" | ">" | "|-" | ">-" | "|+" | ">+") {
        let indent = content.len() - content.trim_start_matches([' ', '\t']).len();
        Some(indent + 1)
    } else {
        None
    }
}

fn sanitize_line(line: &str, found: &mut Vec<DetectedSecret>) -> String {
    let step = redact_url_userinfo(line, found);
    let step = redact_sensitive_assignments(&step, found);
    // Self-identifying key shapes (sk-…, ghp_…, AKIA…) need no key
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
    let tokens = reference_token_ranges(line);
    let mut out = String::with_capacity(line.len());
    let mut idx = 0usize;
    while idx < line.len() {
        let Some((key_start, key_end)) = next_sensitive_key(line, idx) else {
            out.push_str(&line[idx..]);
            return out;
        };
        // `[secret:sec_…]` is the runtime's own custody token. Its inner word
        // is a name in a protocol, not a credential assignment; treating it as
        // one would rewrite the reference and break resume and tool use.
        if let Some((_start, end)) = tokens
            .iter()
            .find(|(start, end)| key_start >= *start && key_start < *end)
            .copied()
        {
            out.push_str(&line[idx..end]);
            idx = end.max(key_start + 1);
            continue;
        }
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
        // Optional closing quote on a JSON/YAML/markdown key: "password", `password`
        if matches!(bytes.get(cursor), Some(b'"') | Some(b'\'') | Some(b'`')) {
            cursor += 1;
        }
        cursor = skip_spaces(bytes, cursor);
        // Separator: `=` or `:` … or plain whitespace for CLI flags
        // (`--password hunter2`), which only counts when the key was a flag.
        // A flag is `--password value` or `-password value`. A hyphen *inside*
        // a word (`non-secret description`) is not a flag: it would turn the
        // word after it into a credential value.
        let before_key = &line[..key_start];
        let flag_style = before_key.ends_with("--")
            || (before_key.ends_with('-')
                && !before_key[..before_key.len() - 1]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric()));
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
        let mut credential_scheme = false;
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
                credential_scheme = true;
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
        let value_is_reference = reference_token_ranges(value)
            .iter()
            .any(|(start, end)| *start == 0 && *end == value.len());
        if !value_is_reference
            && is_credential_value(&key, value, quoted, code_line, credential_scheme)
        {
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
        "pass",
        "db_pass",
        "db_password",
        "database_password",
        "mysql_root_password",
        "postgres_password",
        "secret",
        "secrets",
        "token",
        "credential",
        "credentials",
        "pwd",
        // The bare hand-off form (`key:mgzk…`). Its value shape is checked
        // strictly at classification time; this entry only lets the scanner
        // find the position.
        "key",
    ];
    let lower = line.to_ascii_lowercase();
    let mut best: Option<(usize, usize)> = None;
    for key in KEYS {
        let mut search = from;
        while let Some(rel) = lower[search..].find(key) {
            let start = search + rel;
            let end = start + key.len();
            if is_identifier_boundary(&lower, start, end) {
                // A bare `key` must stand on its own; `SERVICE_KEY` is an
                // ordinary compound and is not this lane.
                if *key == "key" && start > 0 && matches!(lower.as_bytes()[start - 1], b'_' | b'-')
                {
                    search = start + 1;
                    if search >= lower.len() {
                        break;
                    }
                    continue;
                }
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
        .is_none_or(|c| !c.is_ascii_alphanumeric())
        // A JSON-encoded newline/tab is still a line start for the text that
        // will be decoded on the other side. Without this, `\nTOKEN` reads
        // `nTOKEN` as one identifier and the value escapes the detector.
        || is_json_escape_boundary(lower, start);
    let after_ok = lower[end..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    before_ok && after_ok
}

/// Whether `start` follows a JSON escape that represents whitespace. The
/// scanner works on the encoded bytes, so the two characters `\` and `n` are
/// the line break the decoded document will contain.
fn is_json_escape_boundary(lower: &str, start: usize) -> bool {
    let bytes = lower.as_bytes();
    start >= 2 && bytes[start - 2] == b'\\' && matches!(bytes[start - 1], b'n' | b'r' | b't')
}

/// The value after a separator: `(start, end, quoted)`.
fn value_span(line: &str, from: usize) -> Option<(usize, usize, bool)> {
    let bytes = line.as_bytes();
    match bytes.get(from) {
        None | Some(b'\n') | Some(b'\r') => None,
        Some(&q @ (b'"' | b'\'' | b'`')) => {
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
            // Unquoted values are ASCII tokens. A non-ASCII character is
            // prose punctuation (Chinese comma, full stop, full-width colon)
            // or the start of a sentence, never part of a bare credential;
            // consuming it would both miss the exact span and destroy the
            // text around the value when it is redacted.
            let end = line[from..]
                .char_indices()
                .find(|(_, c)| {
                    c.is_whitespace()
                        || !c.is_ascii()
                        || matches!(c, ',' | '}' | ')' | ';' | '\'' | '"' | '`')
                })
                .map(|(rel, _)| from + rel)
                .unwrap_or(line.len());
            (end > from).then_some((from, end, false))
        }
    }
}

/// Does this value look like a credential rather than code or a placeholder?
fn is_credential_value(
    key: &str,
    value: &str,
    quoted: bool,
    code_line: bool,
    credential_scheme: bool,
) -> bool {
    let trimmed = value.trim();
    if trimmed.len() < MIN_SECRET_LEN {
        return false;
    }
    // A documentation placeholder is a shape, not a credential. Treating
    // `<YOUR_API_TOKEN>` or `{{API_TOKEN}}` as a value would redact the
    // document the person is asking about (and used to make an accepted
    // steer undeliverable).
    if is_template_value(trimmed) || is_placeholder_value(trimmed) {
        return false;
    }
    if is_bare_key_name(key) && !is_opaque_key_value(trimmed) {
        return false;
    }
    const NON_SECRETS: &[&str] = &[
        "[redacted]",
        "<secret>",
        "<redacted>",
        "changeme",
        "demo-password",
        "test-password",
        "example-password",
        "sample-password",
        "dummy-password",
        "password",
        "secret",
        "string",
        "boolean",
        "number",
        "undefined",
        "required",
        "invalid",
        "optional",
        "enabled",
        "disabled",
        "true",
        "false",
        "none",
        "null",
        "bearer",
        "basic",
        "completed",
        "pending",
        "running",
        "failed",
        "success",
        "valid",
    ];
    let lower = trimmed.to_ascii_lowercase();
    if NON_SECRETS.contains(&lower.as_str()) {
        return false;
    }
    if is_non_secret_prose(trimmed) {
        return false;
    }
    // A scheme word is explicit credential context: a dotted opaque token
    // (Bearer abc.def.ghi) is a token, not a code expression.
    if !credential_scheme && is_code_expression(trimmed) {
        return false;
    }
    // On a line that reads as source code, a bare value is an identifier or a
    // language expression, not the credential literal. Quoted literals stay
    // eligible: Password: "hunter2" is a value the source really contains.
    //
    // Only a *bare identifier* is demoted, not every bare token: the storage
    // scrubber runs on prose too, and password: p@ss,with{braces} is prose
    // whose value happens to contain code punctuation. It must still be
    // redacted.
    if code_line && !quoted && is_bare_identifier(trimmed) {
        return false;
    }
    true
}

/// The bare word `key` is a common English noun. It is accepted as a
/// credential key only when it stands alone (`key: …`, `key=…`), never as the
/// suffix of an ordinary compound such as `SERVICE_KEY`.
fn is_bare_key_name(key: &str) -> bool {
    key.trim_matches(['"', '\'', '`'])
        .eq_ignore_ascii_case("key")
}

/// A bare `key:` hand-off must carry an opaque token, not a word or a
/// placeholder. This keeps `key: important` and `key: <YOUR_API_TOKEN>`
/// readable while catching `key:mgzk…` (32 mixed alphanumerics).
fn is_opaque_key_value(value: &str) -> bool {
    let value = value.trim();
    // A hand-off token is one opaque run. Separators are how ordinary
    // business identifiers are written (`order-2026-11-26-0001`), so the bare
    // `key:` lane refuses them; a real token that needs them is still caught
    // by the ordinary assignment rules when its key names the credential.
    if value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '-' | '_' | '/' | ':' | '.'))
    {
        return false;
    }
    let alpha = value.chars().filter(char::is_ascii_alphabetic).count();
    let digits = value.chars().filter(char::is_ascii_digit).count();
    let mut seen = std::collections::HashSet::new();
    value.chars().for_each(|c| {
        seen.insert(c);
    });
    value.len() >= 16
        // An upper-case prefix plus a date/counter (`ORD202611260001`) is an
        // identifier, not an opaque token: a real hand-off mixes enough
        // characters that a short identifier cannot reach this entropy.
        && alpha >= 8
        && digits >= 2
        && seen.len() >= 10
        && !is_template_value(value)
        && !is_placeholder_value(value)
}

/// Placeholder shapes that are not enclosed in `<...>`/`{{...}}`/`${...}`.
/// They name a credential without carrying one.
fn is_placeholder_value(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "your_api_token"
            | "your_token"
            | "your-api-token"
            | "your-token"
            | "replace_me"
            | "replace-me"
            | "redacted"
            | "secret_here"
            | "secret-here"
    )
}

/// A value used for structured JSON/YAML key decisions. It does not know the
/// source line, so it is deliberately the strict subset of the text rules.
pub(crate) fn is_secret_like_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.len() < MIN_SECRET_LEN || is_template_value(trimmed) || is_placeholder_value(trimmed)
    {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "[redacted]"
            | "<secret>"
            | "<redacted>"
            | "changeme"
            | "demo-password"
            | "test-password"
            | "example-password"
            | "sample-password"
            | "dummy-password"
            | "password"
            | "secret"
            | "string"
            | "boolean"
            | "number"
            | "undefined"
            | "required"
            | "invalid"
            | "optional"
            | "enabled"
            | "disabled"
            | "true"
            | "false"
            | "none"
            | "null"
            | "bearer"
            | "basic"
            | "completed"
            | "pending"
            | "running"
            | "failed"
            | "success"
            | "valid"
    ) {
        return false;
    }
    if is_non_secret_prose(trimmed) {
        return false;
    }
    if is_code_expression(trimmed) {
        return false;
    }
    true
}

/// A short UI/status phrase under a sensitive key is not a credential. The
/// hint list catches common labels (Enter your password, The token has
/// expired) without treating every space as prose: a passphrase whose first
/// word is not a UI verb still gets redacted.
fn is_non_secret_prose(value: &str) -> bool {
    if !value.chars().any(char::is_whitespace) {
        return false;
    }
    let first = value
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "enter"
            | "your"
            | "the"
            | "please"
            | "new"
            | "confirm"
            | "current"
            | "forgot"
            | "reset"
            | "paste"
            | "provide"
            | "specify"
            | "a"
            | "an"
            | "this"
            | "that"
            | "not"
            | "no"
            | "if"
            | "when"
            | "use"
            | "set"
    )
}

/// A single language-level identifier (`redisPassword`, `_token`, `apiKey`).
///
/// This is a shape, not a verdict: the verdict comes from the line context
/// ([`looks_like_code`]) plus the value position. The shape is only used to
/// keep code identifiers out of a code line's secret positions.
fn is_bare_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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
    // A bare CamelCase type name (`String`, `SecretString`). A digit means it
    // is an opaque token, not a type.
    value.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && value.chars().all(|c| c.is_ascii_alphabetic())
        && value.chars().any(|c| c.is_ascii_lowercase())
}

/// Heuristic: does this line read as source code declaring or using a field?
/// Used only to demote BARE identifiers; quoted literals are still redacted.
///
/// A struct-literal field is one line of a larger expression, so the line
/// alone often has no brace: `\tPassword: redisPassword,` is code because it is
/// indented, has a field separator, and ends with the field comma. A comment
/// is deliberately not code — a credential in a comment is still a credential.
fn looks_like_code(line: &str) -> bool {
    if is_comment_line(line) {
        return false;
    }
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
        ":=",
        "{",
        "}",
        "(",
        ")",
        "#[",
    ];
    if MARKERS.iter().any(|m| line.contains(m)) {
        return true;
    }
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let indented = trimmed.len() != trimmed.trim_start().len();
    indented
        && trimmed.trim_end().ends_with(',')
        && (trimmed.contains(':') || trimmed.contains('='))
}

/// A line whose credential-shaped text is prose/comment, not code structure.
fn is_comment_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("-- ")
        || trimmed.starts_with("--\t")
        || (trimmed.starts_with('#') && !trimmed.starts_with("#["))
        || (trimmed.starts_with(';')
            && trimmed[1..].chars().next().is_some_and(char::is_whitespace))
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
        if value.len() < MIN_SECRET_LEN
            || value.len() > MAX_REGISTERED_VALUE_LEN
            || entry.len() >= MAX_REGISTERED_PER_SESSION
        {
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
            out = scrub_standalone(&out, value);
        }
    }
    out
}

/// Replace `value` only where it stands alone.
///
/// The registry exists to catch a credential the model repeated in prose.
/// Its exact-value match is not an identity proof: a local database password
/// can be the project name (`password=devorder`), and rewriting every
/// occurrence then corrupts Go import paths, file names and identifiers in
/// the durable transcript — which resume replays to the model, so the agent
/// reads `[REDACTED]` where a module path belongs and writes it into source.
///
/// A match inside a longer path/identifier token is a different token, so it
/// is left byte-identical. A standalone occurrence — prose, `KEY=value`, a
/// quoted assignment — is still scrubbed.
fn scrub_standalone(text: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(rel) = text[cursor..].find(value) {
        let start = cursor + rel;
        let end = start + value.len();
        out.push_str(&text[cursor..start]);
        if is_inside_token(text, start, end) {
            out.push_str(&text[start..end]);
        } else {
            out.push_str(SECRET_PLACEHOLDER);
        }
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Whether `text[start..end]` continues a path/identifier token on either
/// side. `.` counts as a token character only when it joins two word
/// characters (`my.devorder`), not when it ends a sentence (`is devorder.`).
fn is_inside_token(text: &str, start: usize, end: usize) -> bool {
    fn word(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '\\')
    }
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    let before_dot = before == Some('.')
        && text[..start - 1]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    let after_dot = after == Some('.')
        && text[end + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    before.is_some_and(word) || after.is_some_and(word) || before_dot || after_dot
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

    /// A JSON-encoded command keeps its line breaks as the two bytes `\n`.
    /// The key after that escape is at a logical line start, so the detector
    /// must treat the escape as a boundary instead of reading it as part of
    /// the identifier.
    #[test]
    fn redacts_a_secret_after_a_json_escaped_line_break() {
        let token = "canary-7f3a9c2e5b8d4f6a1c0e8b7d";
        for text in [
            format!("cmd: echo ok\\nTOKEN='{token}'\\nrun"),
            format!("cmd: echo ok\\tTOKEN='{token}'\\trun"),
        ] {
            let (out, found) = sanitize_model_visible(&text);
            assert!(!out.contains(token), "{text:?} -> {out:?}");
            assert_eq!(found.len(), 1, "{text:?} -> {found:?}");
        }
        // A real line break already works and must keep working.
        let real = format!("cmd: echo ok\nTOKEN='{token}'\nrun");
        let (out, found) = sanitize_model_visible(&real);
        assert!(!out.contains(token), "{out:?}");
        assert_eq!(found.len(), 1);
        // `nTOKEN` without an escape is an ordinary identifier: not a key.
        let ordinary = format!("cmd: nTOKEN='{token}'");
        let (out, _) = sanitize_model_visible(&ordinary);
        assert!(
            out.contains(token),
            "ordinary identifier was rewritten: {out:?}"
        );
    }

    /// `key: <value>` is a common hand-off form (`key:mgzk…`, no space). The
    /// bare word is too ambiguous for the general key list, so it is accepted
    /// only with an opaque-token value; prose and config labels stay readable.
    #[test]
    fn detects_a_bare_key_assignment_with_an_opaque_value() {
        let token = "mgzk1gr8k7bfm6z6t8ft6tdxd8ydmm5t";
        for text in [
            format!("然后这是key:{token} 要用search api 再测试一次"),
            format!("key: {token}"),
            format!("key={token}"),
            format!("  \"key\": \"{token}\""),
        ] {
            let (out, found) = sanitize_model_visible(&text);
            assert!(!out.contains(token), "{text:?} -> {out:?}");
            assert_eq!(found.len(), 1, "{text:?} -> {found:?}");
            let findings = detect_secrets(&text);
            assert_eq!(findings.len(), 1, "{text:?} -> {findings:?}");
        }
        for text in [
            "the key: important",
            "key point: measure twice, cut once",
            "SERVICE_KEY=production",
            "key: value",
            "key: <YOUR_API_TOKEN>",
            "key: order-2026-11-26-0001",
            "key: ORD202611260001",
            "key: 202611260001234",
            "key: user_42_profile_settings",
        ] {
            let (out, found) = sanitize_model_visible(text);
            assert_eq!(
                out, text,
                "ordinary text was rewritten: {text:?} -> {out:?}"
            );
            assert!(found.is_empty(), "{text:?} -> {found:?}");
            assert!(detect_secrets(text).is_empty(), "{text:?}");
        }
    }

    /// Documentation placeholders are shapes, not credentials. Refusing or
    /// redacting them destroys the document the person is asking about.
    #[test]
    fn leaves_document_placeholders_alone() {
        for text in [
            "Authorization: Bearer <YOUR_API_TOKEN>",
            "Authorization: Bearer {{API_TOKEN}}",
            "请将 <YOUR_API_TOKEN> 替换为实际 Token",
            "token: <token>",
        ] {
            let (out, found) = sanitize_model_visible(text);
            assert_eq!(out, text, "placeholder was rewritten: {out:?}");
            assert!(found.is_empty(), "{text:?} -> {found:?}");
            assert!(detect_secrets(text).is_empty(), "{text:?}");
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

    /// REGRESSION P1-2. A language-level identifier is not a credential just
    /// because it sits on the right of `Password:`. The real session
    /// `9db65d21` persisted `[REDACTED] := os.Getenv(...)` and
    /// `Password: [REDACTED]` because the line-level code heuristic missed a
    /// struct-literal field and the value was registered as a secret.
    #[test]
    fn ordinary_code_identifiers_are_not_secrets() {
        let cases = [
            "redisPassword := os.Getenv(\"TEST_REDIS_PASSWORD\")\n",
            "accessToken := os.Getenv(\"ACCESS_TOKEN\")\n",
            "clientSecret := os.Getenv(\"CLIENT_SECRET\")\n",
            "apiKey := config.APIKey\n",
            "client := Config{\n\tAddr:     redisAddr,\n\tPassword: redisPassword,\n}\n",
            "client := Config{ Password: redisPassword, Token: accessToken }\n",
            "let client = Config { password: redis_password, token: access_token };\n",
            "config = Config(password=redis_password, token=access_token)\n",
            "const client = { password: redisPassword, token: accessToken };\n",
        ];
        for input in cases {
            let (out, found) = sanitize_model_visible(input);
            assert_eq!(out, input, "code was rewritten: {input:?}");
            assert!(
                found.is_empty(),
                "identifier registered as secret: {found:?}"
            );
        }
    }

    /// REGRESSION P1-2. A code identifier that was never redacted must not be
    /// registered, and a clean registry must leave a later transcript
    /// byte-identical.
    #[test]
    fn ordinary_identifiers_do_not_enter_the_session_registry() {
        let session = "sess-code-identifiers";
        clear_session_secrets(session);
        let (_, found) = sanitize_model_visible("client := Config{ Password: redisPassword }\n");
        assert!(found.is_empty(), "identifier detected: {found:?}");
        register_session_secrets(session, &found);
        assert_eq!(registered_secret_count(session), 0);

        let text = "client := Config{ Password: redisPassword }\n";
        assert_eq!(
            scrub_registered_secrets(session, text),
            text,
            "a clean registry must not rewrite code"
        );
        clear_session_secrets(session);
    }

    /// Comment lines carry no code structure: a credential-shaped value in a
    /// comment must still be redacted, not treated as a field expression.
    #[test]
    fn credentials_in_comments_are_still_redacted() {
        for input in [
            "// password: hunter2-abc\n",
            "# password: hunter2-abc\n",
            "/* password: hunter2-abc */\n",
            "-- password: hunter2-abc\n",
        ] {
            let (out, found) = sanitize_model_visible(input);
            assert!(!out.contains("hunter2-abc"), "comment leaked: {out:?}");
            assert_eq!(found.len(), 1, "comment value not registered: {out:?}");
        }
    }

    /// Dogfood A regression: a hyphen inside a word is not a CLI flag. The
    /// prose `non-secret description` was redacted to `non-secret
    /// [REDACTED]` because the detector read `-secret` as a flag.
    #[test]
    fn hyphenated_prose_is_not_a_credential_flag() {
        for input in [
            "// Describe returns a non-secret description of the configuration.\n",
            "// This is a non-secret value.\n",
            "the non-secret description is public\n",
        ] {
            let (out, found) = sanitize_model_visible(input);
            assert_eq!(out, input, "prose rewritten: {out:?}");
            assert!(found.is_empty(), "prose registered: {found:?}");
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

    /// Markdown inline code is how an assistant naturally writes about types
    /// and fields. A backtick must delimit the value, or `password: String`
    /// gets redacted and the closing backtick swallowed — which is exactly
    /// what mangled a correct answer in the Gate 1 daemon smoke.
    #[test]
    fn markdown_inline_code_is_not_a_credential() {
        for input in [
            "- `password: String`",
            "- `token: Option<String>`",
            "the field `api_key: string` is required",
        ] {
            assert_eq!(sanitize(input), input, "markdown mangled: {input:?}");
        }
        // A real secret inside backticks is still redacted, and the closing
        // backtick survives.
        let out = sanitize("`password: hunter2-abc`");
        assert_eq!(out, "`password: [REDACTED]`");
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

    /// The R011 accident: a low-entropy password that is also a project name
    /// (`password=devorder`) registered the shared word, and the durable
    /// scrubber then rewrote every Go import path through the project. The
    /// value is a secret only at the position it was detected in; an exact
    /// match that is part of a longer path/identifier is a different token.
    #[test]
    fn registered_values_do_not_rewrite_paths_or_identifiers() {
        let session = "sess-path-identity";
        clear_session_secrets(session);
        register_session_secrets(
            session,
            &[DetectedSecret {
                value: "devorder".into(),
                key: "password".into(),
            }],
        );

        for intact in [
            "\tcoreerr \"github.com/dengmengmian/devorder/internal/core/errors\"",
            "github.com/dengmengmian/devorder",
            "cmd/devorder-app",
            "my_devorder_service",
            "github.com/dengmengmian/devorder.git",
            "my.devorder",
            r"C:\src\devorder\internal",
        ] {
            assert_eq!(
                scrub_registered_secrets(session, intact),
                intact,
                "a path/identifier was rewritten: {intact:?}"
            );
        }

        for redacted in [
            "the database password is devorder",
            "dbname=devorder",
            "password: \"devorder\"",
            "devorder",
        ] {
            assert!(
                scrub_registered_secrets(session, redacted).contains(SECRET_PLACEHOLDER),
                "a standalone credential occurrence must still be scrubbed: {redacted:?}"
            );
        }
        clear_session_secrets(session);
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

    /// F-03: the shapes the audit found outside the old key list.
    #[test]
    fn pem_common_pass_and_multiline_shapes_are_redacted() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234567890abcdef\n-----END RSA PRIVATE KEY-----";
        let out = sanitize(pem);
        assert_eq!(out, SECRET_PLACEHOLDER, "PEM must be one hidden value");
        assert!(!out.contains("MIIEowIBAAKCAQEA1234567890abcdef"));

        for input in [
            "DB_PASS=hunter2-abc",
            "PASS=hunter2-abc",
            "database_password=hunter2-abc",
            "mysql_root_password=hunter2-abc",
            "{\"secrets\": \"hunter2-abc\"}",
            "{\"credentials\": \"hunter2-abc\"}",
        ] {
            let out = sanitize(input);
            assert!(!out.contains("hunter2-abc"), "missed {input:?} -> {out:?}");
        }

        let yaml = "password: |\n  hunter2-abc\n  more-secret\nnext: keep";
        let out = sanitize(yaml);
        assert!(!out.contains("hunter2-abc"), "{out}");
        assert!(!out.contains("more-secret"), "{out}");
        assert!(out.contains("next: keep"), "{out}");
    }

    /// F-06: ordinary status/prose values under a sensitive word are not
    /// credentials, and a dotted opaque token under a scheme is.

    #[test]
    fn equality_is_not_a_password_assignment() {
        let line = "\treturn password == \"demo-password\"";
        assert_eq!(sanitize(line), line);
        let assignment = "\tpassword = \"demo-password\"";
        assert_eq!(sanitize(assignment), assignment);
    }

    #[test]
    fn read_file_line_numbers_do_not_defeat_block_scalar_redaction() {
        let input = "     1\tdb_password: |\n     2\t  hunter2-abc\n     3\t  more-secret\n     4\tsafe: yes\n";
        let out = sanitize(input);
        assert!(!out.contains("hunter2-abc"), "{out}");
        assert!(!out.contains("more-secret"), "{out}");
        assert!(out.contains("safe: yes"), "{out}");
    }

    #[test]
    fn shell_header_keeps_its_quote_boundary() {
        let out = sanitize("curl -H 'X-Api-Key: hunter2-abc' https://x");
        assert!(!out.contains("hunter2-abc"), "{out}");
        assert!(
            out.contains("' https://x"),
            "closing quote must survive: {out}"
        );
    }

    #[test]
    fn status_prose_and_dotted_bearer_are_classified() {
        assert_eq!(
            sanitize("authorization: required"),
            "authorization: required"
        );
        assert_eq!(sanitize("token: invalid"), "token: invalid");
        assert_eq!(
            sanitize("{\"password\": \"Enter your password\"}"),
            "{\"password\": \"Enter your password\"}"
        );
        let dotted = "Authorization: Bearer abcdef0123456789.abcdef0123456789";
        let out = sanitize(dotted);
        assert!(!out.contains("abcdef0123456789"), "{out}");
        assert!(out.contains("Bearer"), "{out}");
    }

    #[test]
    fn only_workspace_dotenv_files_are_targets() {
        for (input, expected) in [
            (".env", Some(".env")),
            (".env.local", Some(".env.local")),
            (".env.production", Some(".env.production")),
            ("./.env", Some(".env")),
            ("apps/web/.env", Some("apps/web/.env")),
            (r"apps\web\.env", Some("apps/web/.env")),
            ("apps//web/.env", Some("apps/web/.env")),
            ("配置 目录/.env", Some("配置 目录/.env")),
        ] {
            assert_eq!(
                normalize_dotenv_target(input).as_deref(),
                expected,
                "{input}"
            );
        }
        for input in [
            "",
            ".envrc",
            "config.toml",
            "../.env",
            "/etc/.env",
            r"C:\project\.env",
            "a/../../.env",
            "dotenv",
        ] {
            assert_eq!(normalize_dotenv_target(input), None, "{input}");
        }
    }

    #[test]
    fn the_users_own_text_names_the_targets_a_capture_may_write() {
        for (text, expected) in [
            ("请把凭据配置到 .env：", vec![".env"]),
            ("写到 apps/web/.env.local 里", vec!["apps/web/.env.local"]),
            (
                "用 `./.env` 和 .env.production",
                vec![".env.production", ".env"],
            ),
            ("配置到.env中", vec![".env"]),
            ("see (.env), then continue", vec![".env"]),
        ] {
            let mut got = dotenv_targets_in(text);
            got.sort();
            let mut expected: Vec<String> = expected.into_iter().map(String::from).collect();
            expected.sort();
            assert_eq!(got, expected, "{text}");
        }
        for text in [
            "把 key 配置进去",
            "README.env is documentation",
            "my.env is not a dotenv name",
            "https://example.com/.env",
            "/etc/.env",
        ] {
            assert!(dotenv_targets_in(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn only_explicit_assignments_name_a_field() {
        for (text, expected) in [
            ("DEMO_API_KEY=CANARY", vec!["DEMO_API_KEY"]),
            ("export DB_PASSWORD = x", vec!["DB_PASSWORD"]),
            ("api_key=\"abc\"", vec!["api_key"]),
        ] {
            let mut got = dotenv_fields_in(text);
            got.sort();
            let mut expected: Vec<String> = expected.into_iter().map(String::from).collect();
            expected.sort();
            assert_eq!(got, expected, "{text}");
        }
        for text in [
            "密码是 password: hunter2",
            "值在字段 DEMO_API_KEY 里",
            "if (a == b) {}",
            "a >= b",
            "FOO=>bar",
            "1BAD=value",
        ] {
            assert!(dotenv_fields_in(text).is_empty(), "{text}");
        }
    }
}
