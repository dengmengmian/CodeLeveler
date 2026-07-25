//! Pure memory-candidate extraction (no I/O except optional path reads).
//!
//! System-proposed candidates must still go through
//! [`crate::MemoryStore::accept`] (user consent) before becoming active
//! entries. Extractors never write durable memory themselves.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{MemoryError, now_rfc3339, slugify};

/// How a candidate was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// User said "remember …" / "记住：…".
    UserExplicit,
    /// Filesystem / project signal (e.g. package manager lockfile).
    SystemPropose,
}

/// Stable category for structured keys and suppress logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    Preference,
    PackageManager,
    Free,
}

/// A pending memory proposal. Not durable project memory until accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCandidate {
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub kind: CandidateKind,
    /// Stable key for dedup / suppress (e.g. `package_manager`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Content fingerprint for suppress-after-reject.
    pub fingerprint: String,
    pub source: CandidateSource,
    pub created_at: String,
}

impl MemoryCandidate {
    /// Build a candidate with slug id and fingerprint derived from kind/key/body.
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        kind: CandidateKind,
        key: Option<String>,
        source: CandidateSource,
        tags: Vec<String>,
    ) -> Result<Self, MemoryError> {
        let title = title.into().trim().to_string();
        let body = body.into().trim().to_string();
        if title.is_empty() || body.is_empty() {
            return Err(MemoryError::Invalid(
                "candidate title and body are required".into(),
            ));
        }
        if looks_like_secret(&title) || looks_like_secret(&body) {
            return Err(MemoryError::Invalid(
                "refusing candidate that looks like a secret".into(),
            ));
        }
        let fingerprint = fingerprint_of(kind, key.as_deref(), &title, &body);
        let id_base = key
            .as_deref()
            .map(|k| format!("cand-{k}"))
            .unwrap_or_else(|| format!("cand-{}", slugify(&title)));
        let id = slugify(&id_base);
        Ok(Self {
            id,
            title,
            body,
            tags,
            kind,
            key,
            fingerprint,
            source,
            created_at: now_rfc3339(),
        })
    }
}

/// Stable fingerprint used for suppress-after-reject and dedup.
pub fn fingerprint_of(kind: CandidateKind, key: Option<&str>, title: &str, body: &str) -> String {
    let kind_s = match kind {
        CandidateKind::Preference => "preference",
        CandidateKind::PackageManager => "package_manager",
        CandidateKind::Free => "free",
    };
    let raw = format!(
        "{kind_s}|{}|{}|{}",
        key.unwrap_or(""),
        title.trim(),
        body.trim()
    );
    // Short stable hex-ish hash (FNV-1a 64) — not cryptographic.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in raw.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Heuristic: refuse API keys / tokens in candidate text.
pub fn looks_like_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "api_key",
        "apikey",
        "secret_key",
        "private_key",
        "-----begin",
        "sk-",
        "ghp_",
        "gho_",
        "xoxb-",
        "bearer ",
        "authorization: ",
    ];
    if MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // Long base64-ish blob.
    let alnum: String = text.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    alnum.len() >= 40
        && text.contains('=')
        && alnum
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
}

/// Parse explicit user intent to remember something for later sessions.
///
/// Representative phrases (not full NLU):
/// - `记住：用 pnpm` / `记住 用 pnpm`
/// - `请记住：以后提交按路径 add`
/// - `remember: always use WorkspaceWrite`
/// - `always use pnpm` / `以后都用 pnpm`
pub fn parse_explicit_remember_intent(text: &str) -> Option<MemoryCandidate> {
    let t = text.trim();
    if t.is_empty() || looks_like_secret(t) {
        return None;
    }

    let body = extract_remember_body(t)?;
    if body.chars().count() < 2 || body.chars().count() > 400 {
        return None;
    }
    if looks_like_secret(&body) {
        return None;
    }

    let title = preference_title(&body);
    MemoryCandidate::new(
        title,
        body,
        CandidateKind::Preference,
        None,
        CandidateSource::UserExplicit,
        vec!["preference".into(), "user-explicit".into()],
    )
    .ok()
}

fn extract_remember_body(text: &str) -> Option<String> {
    let trimmed = text.trim();

    // Chinese: 请记住 / 记住 + optional colon
    for prefix in ["请记住", "记住"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let rest = rest.trim_start_matches(['：', ':', ' ', '\t']).trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }

    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["please remember", "remember"] {
        if let Some(idx) = lower.find(prefix)
            && idx == 0
        {
            let rest = trimmed[prefix.len()..]
                .trim_start_matches([':', ' ', '\t', '：'])
                .trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }

    // Soft explicit preferences often stated as lasting rules.
    if let Some(rest) = strip_ci_prefix(trimmed, "always use ") {
        return Some(format!("Always use {rest}"));
    }
    if let Some(rest) = strip_prefix_chars(trimmed, "以后都用") {
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some(format!("以后都用{rest}"));
        }
    }
    if let Some(rest) = strip_prefix_chars(trimmed, "以后都") {
        let rest = rest.trim();
        if !rest.is_empty() {
            return Some(format!("以后都{rest}"));
        }
    }

    None
}

fn strip_ci_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let lower = text.to_ascii_lowercase();
    let p = prefix.to_ascii_lowercase();
    if lower.starts_with(&p) {
        Some(text[prefix.len()..].trim())
    } else {
        None
    }
}

fn strip_prefix_chars<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.strip_prefix(prefix)
}

fn preference_title(body: &str) -> String {
    let one_line: String = body.lines().next().unwrap_or(body).trim().to_string();
    let short: String = one_line.chars().take(48).collect();
    if short.is_empty() {
        "偏好".into()
    } else {
        format!("偏好：{short}")
    }
}

/// Detect package manager from lockfiles / `package.json#packageManager`.
///
/// At most one candidate; prefers lockfiles: pnpm → yarn → npm, else
/// `packageManager` field.
pub fn detect_package_manager(root: &Path) -> Option<MemoryCandidate> {
    let pm = package_manager_from_root(root)?;
    let title = format!("包管理器：{pm}");
    let body = format!(
        "This repository uses `{pm}` as its package manager. Prefer `{pm}` \
         (e.g. `{pm} install`, `{pm} run …`) over other Node package managers \
         unless the user explicitly overrides for a one-off command."
    );
    MemoryCandidate::new(
        title,
        body,
        CandidateKind::PackageManager,
        Some("package_manager".into()),
        CandidateSource::SystemPropose,
        vec!["package_manager".into(), pm.to_string()],
    )
    .ok()
}

/// Return `pnpm` / `yarn` / `npm` when project signals exist.
pub fn package_manager_from_root(root: &Path) -> Option<&'static str> {
    if root.join("pnpm-lock.yaml").is_file() {
        return Some("pnpm");
    }
    if root.join("yarn.lock").is_file() {
        return Some("yarn");
    }
    if root.join("package-lock.json").is_file() {
        return Some("npm");
    }
    if let Some(pm) = package_manager_field(root) {
        return Some(pm);
    }
    // package.json alone is not enough to propose a manager.
    None
}

fn package_manager_field(root: &Path) -> Option<&'static str> {
    let path = root.join("package.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let field = v.get("packageManager")?.as_str()?;
    let name = field.split('@').next().unwrap_or(field).trim();
    match name {
        "pnpm" => Some("pnpm"),
        "yarn" => Some("yarn"),
        "npm" => Some("npm"),
        "bun" => Some("npm"), // treat as npm-compatible signal only for install scripts
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn explicit_chinese_remember_colon() {
        let c = parse_explicit_remember_intent("记住：用 pnpm").expect("candidate");
        assert_eq!(c.source, CandidateSource::UserExplicit);
        assert_eq!(c.kind, CandidateKind::Preference);
        assert!(c.body.contains("pnpm"), "{}", c.body);
        assert!(c.title.contains("pnpm") || c.body.contains("pnpm"));
    }

    #[test]
    fn explicit_english_remember() {
        let c = parse_explicit_remember_intent("remember: always use WorkspaceWrite")
            .expect("candidate");
        assert!(
            c.body.to_ascii_lowercase().contains("workspacewrite")
                || c.body.contains("WorkspaceWrite")
        );
    }

    #[test]
    fn soft_always_use_and_chinese_prefer() {
        assert!(parse_explicit_remember_intent("always use pnpm").is_some());
        let c = parse_explicit_remember_intent("以后都用 pnpm").expect("c");
        assert!(c.body.contains("pnpm"));
    }

    #[test]
    fn secrets_rejected() {
        assert!(parse_explicit_remember_intent("记住：api_key=sk-abc123secret").is_none());
        assert!(
            MemoryCandidate::new(
                "key",
                "Authorization: Bearer supersecrettokenvalue",
                CandidateKind::Free,
                None,
                CandidateSource::UserExplicit,
                vec![],
            )
            .is_err()
        );
    }

    #[test]
    fn pnpm_lockfile_one_candidate() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: '9'\n").unwrap();
        let c = detect_package_manager(dir.path()).expect("pm");
        assert_eq!(c.kind, CandidateKind::PackageManager);
        assert_eq!(c.key.as_deref(), Some("package_manager"));
        assert!(c.body.contains("pnpm"));
        // Only one logical candidate from this root.
        assert_eq!(package_manager_from_root(dir.path()), Some("pnpm"));
    }

    #[test]
    fn package_manager_field_without_lockfile() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"name":"x","packageManager":"pnpm@9.0.0"}"#,
        )
        .unwrap();
        assert_eq!(package_manager_from_root(dir.path()), Some("pnpm"));
        assert!(detect_package_manager(dir.path()).is_some());
    }

    #[test]
    fn no_signal_no_candidate() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert!(detect_package_manager(dir.path()).is_none());
    }

    #[test]
    fn yarn_over_npm_when_yarn_lock_present() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("yarn.lock"), "# yarn\n").unwrap();
        fs::write(dir.path().join("package-lock.json"), "{}\n").unwrap();
        assert_eq!(package_manager_from_root(dir.path()), Some("yarn"));
    }
}
