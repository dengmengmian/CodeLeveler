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
    /// LEGACY. Before direct writes existed, an explicit "记住：…" became a
    /// candidate; today that goes straight to active, so nothing new carries
    /// this. Kept so old `pending/` files still deserialize, and shown as
    /// legacy rather than reinterpreted.
    UserExplicit,
    /// LEGACY spelling of [`Self::SystemInferred`], kept for the same reason.
    SystemPropose,
    /// Inferred from how the user talked ("我通常希望输出短一点"). A guess
    /// about a preference, so it needs consent before it becomes memory.
    SystemInferred,
    /// The model proposed it with the `remember` tool. Distinct from a system
    /// guess because the user is approving the MODEL's judgement, and they
    /// deserve to know which one they are looking at.
    AgentProposed,
}

impl CandidateSource {
    /// Whether this is a value only old data carries.
    pub fn is_legacy(self) -> bool {
        matches!(self, Self::UserExplicit | Self::SystemPropose)
    }
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

/// The body of a message that is ONLY a memory command, or `None`.
///
/// This is the parse behind a direct write, so it is deliberately narrow. It
/// recognises a commanding prefix at the very start (`记住：` / `请记住：` /
/// `remember:` / `please remember:`) and nothing else — no "always use X", no
/// "以后都…", because those are how people talk, not how they issue a command,
/// and a parser that rewrites long-term state must not guess.
///
/// Refuses when the message is negated, quotes the phrase, wraps it in code,
/// or carries a second task. Where it cannot be sure, it returns `None` and
/// the message stays an ordinary turn: failing to save is recoverable, while
/// swallowing someone's real task is not.
pub fn parse_direct_memory_command(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || looks_like_secret(t) {
        return None;
    }
    // Code fences or inline code anywhere: the phrase is being shown, not said.
    if t.contains("```") || t.contains('`') {
        return None;
    }
    // A quoted form is being discussed ("解释一下“记住：X”的含义").
    if t.contains('“') || t.contains('"') || t.contains('”') || t.contains('\'') {
        return None;
    }
    let body = strip_command_prefix(t)?;
    let body = body.trim();
    if body.chars().count() < 2 || body.chars().count() > 400 || looks_like_secret(body) {
        return None;
    }
    // A second instruction rides along ("记住 X，然后修复 tests/a.rs"). The
    // task is what matters; the memory can be proposed by the agent later.
    if carries_another_task(body) {
        return None;
    }
    Some(body.to_string())
}

/// Commanding prefixes only, anchored at the first character. A negation
/// (`不要记住…`, `别记住…`) never matches because the prefix is not at the
/// start, which is the whole point of anchoring.
fn strip_command_prefix(text: &str) -> Option<&str> {
    for prefix in ["请记住：", "请记住:", "记住：", "记住:"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    let lower = text.to_ascii_lowercase();
    for prefix in ["please remember:", "remember:"] {
        if lower.starts_with(prefix) {
            return Some(&text[prefix.len()..]);
        }
    }
    None
}

/// Whether the remainder also asks for work to be done.
///
/// Conservative by design: a false positive here costs a direct write (the
/// user can retry with `/remember`), while a false negative eats a real task.
fn carries_another_task(body: &str) -> bool {
    const CONNECTORS: [&str; 6] = ["，然后", "，顺便", "；顺便", ";", ", then ", " and then "];
    if CONNECTORS.iter().any(|c| body.contains(c)) {
        return true;
    }
    // A path or a file extension in a memory body almost always means work.
    body.split_whitespace()
        .any(|w| w.contains('/') && w.contains('.') || w.ends_with(".rs") || w.ends_with(".ts"))
}

/// A SOFT signal: the user described a preference without commanding a save.
///
/// These need consent, so they become candidates. Kept separate from
/// [`parse_direct_memory_command`] precisely so "我通常希望…" can never take
/// the direct path and "记住：…" can never be demoted to a guess.
pub fn parse_inferred_preference(text: &str) -> Option<MemoryCandidate> {
    let t = text.trim();
    if t.is_empty() || looks_like_secret(t) || t.contains('`') {
        return None;
    }
    // Not a soft signal if it is actually a command.
    if strip_command_prefix(t).is_some() {
        return None;
    }
    let body = extract_soft_preference(t)?;
    if body.chars().count() < 2 || body.chars().count() > 400 || looks_like_secret(&body) {
        return None;
    }
    MemoryCandidate::new(
        preference_title(&body),
        body,
        CandidateKind::Preference,
        None,
        CandidateSource::SystemInferred,
        vec!["preference".into(), "inferred".into()],
    )
    .ok()
}

fn extract_soft_preference(text: &str) -> Option<String> {
    // Phrasings that describe a habit rather than issue an order.
    const ZH_HINTS: [&str; 5] = ["我通常", "我一般", "我更喜欢", "以后最好", "以后都"];
    for hint in ZH_HINTS {
        if text.contains(hint) {
            return Some(text.to_string());
        }
    }
    if let Some(rest) = strip_ci_prefix(text, "always use ") {
        return Some(format!("Always use {rest}"));
    }
    if text.to_ascii_lowercase().starts_with("i usually ")
        || text.to_ascii_lowercase().starts_with("i prefer ")
    {
        return Some(text.to_string());
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
/// The title a direct user write gets, derived deterministically from the body.
///
/// One implementation so TUI, Web and CLI cannot drift into three conventions,
/// and never model-generated — a title is not worth a round trip, and a model
/// naming the user's own note would be a second voice in their memory.
///
/// First non-empty line, cut at the first sentence end, capped at 48 chars.
pub fn title_from_body(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    // Sentence enders in both scripts; whichever comes first wins.
    let cut = ['。', '！', '？', '；', '.', '!', '?', ';']
        .iter()
        .filter_map(|c| line.find(*c))
        .min();
    let sentence = match cut {
        Some(at) => &line[..at],
        None => line.as_str(),
    };
    let short: String = sentence.trim().chars().take(48).collect();
    if short.is_empty() {
        "记忆".to_string()
    } else {
        short
    }
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

/// The package manager this repository uses: `pnpm` / `yarn` / `npm` from a
/// lockfile (in that order), else `package.json#packageManager`.
///
/// This is a fact about the working tree, so it is READ on demand and never
/// remembered. The candidate factory that used to turn it into a durable
/// memory was removed: a stored copy is a second source of truth, and it would
/// keep reporting `pnpm` after a project moved to `bun`.
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
    // ---- strict command vs soft signal ----

    /// A message that is only a memory command writes directly. The user
    /// issuing the command IS the authorization; asking them to approve it a
    /// second time is what made the feature feel broken.
    #[test]
    fn a_strict_command_is_a_direct_write() {
        for input in [
            "记住：以后提交前先运行 pnpm lint",
            "记住: 以后提交前先运行 pnpm lint",
            "请记住：TUI 默认保持紧凑",
            "remember: never automatically commit",
            "Please remember: prefer concise terminal output",
        ] {
            let body = parse_direct_memory_command(input)
                .unwrap_or_else(|| panic!("must be a direct write: {input}"));
            assert!(!body.is_empty());
            assert!(!body.starts_with('：') && !body.starts_with(':'), "{body}");
        }
    }

    /// Negated, quoted, code-wrapped, or task-carrying forms must NOT write.
    /// Guessing wrong here either rewrites someone's long-term state or eats
    /// the work they actually asked for.
    #[test]
    fn ambiguous_forms_never_write_directly() {
        for input in [
            // negation
            "不要记住这个",
            "别记住：使用 npm",
            // quoting / discussing the phrase
            "解释一下“记住：使用 npm”是什么意思",
            "解释 \"remember: x\" 的含义",
            // code
            "把源码里的 `remember: foo` 改成 `remember: bar`",
            "代码中包含字符串 `记住：foo`",
            // a second task rides along
            "记住输出要简洁，然后修复 tests/a.rs",
            "记住：使用 pnpm；顺便修复构建错误",
            // not a command at all
            "我通常希望输出短一点",
            "always use pnpm",
            "以后都用 pnpm",
        ] {
            assert!(
                parse_direct_memory_command(input).is_none(),
                "must not write directly: {input}"
            );
        }
    }

    /// A soft signal describes a habit, so it needs consent — and is marked as
    /// the system's inference, not as something the user commanded.
    #[test]
    fn a_soft_signal_becomes_an_inferred_candidate() {
        for input in [
            "我通常希望提交前先运行 pnpm lint。",
            "我一般不希望自动提交",
            "这个项目里我更喜欢函数式写法",
            "always use pnpm",
            "以后都用 pnpm",
        ] {
            let candidate = parse_inferred_preference(input)
                .unwrap_or_else(|| panic!("should be inferred: {input}"));
            assert_eq!(candidate.source, CandidateSource::SystemInferred, "{input}");
            assert_eq!(candidate.kind, CandidateKind::Preference);
        }
    }

    /// The two parsers must not both claim the same input.
    #[test]
    fn a_strict_command_is_never_also_an_inference() {
        let input = "记住：以后提交前先运行 pnpm lint";
        assert!(parse_direct_memory_command(input).is_some());
        assert!(
            parse_inferred_preference(input).is_none(),
            "a command must not also become a candidate to approve"
        );
    }

    /// A credential is refused on both paths, before it can reach a file.
    #[test]
    fn neither_path_accepts_a_secret() {
        let input = "记住：OPENAI_API_KEY=sk-live-abcdefghijklmnopqrstuvwxyz0123456789";
        assert!(parse_direct_memory_command(input).is_none());
        assert!(parse_inferred_preference(input).is_none());
    }

    /// An agent proposal must be distinguishable from a system guess: the user
    /// is approving the model's judgement, and should be told so.
    #[test]
    fn agent_and_system_sources_are_distinct_and_legacy_is_marked() {
        assert_ne!(
            CandidateSource::AgentProposed,
            CandidateSource::SystemInferred
        );
        assert!(!CandidateSource::AgentProposed.is_legacy());
        assert!(!CandidateSource::SystemInferred.is_legacy());
        assert!(CandidateSource::UserExplicit.is_legacy());
        assert!(CandidateSource::SystemPropose.is_legacy());
    }

    /// Old `pending/` files still load.
    #[test]
    fn legacy_sources_still_deserialize() {
        for wire in ["\"user_explicit\"", "\"system_propose\""] {
            let parsed: CandidateSource = serde_json::from_str(wire).expect(wire);
            assert!(parsed.is_legacy(), "{wire}");
        }
        let agent: CandidateSource = serde_json::from_str("\"agent_proposed\"").unwrap();
        assert_eq!(agent, CandidateSource::AgentProposed);
    }

    /// The title a direct write gets is derived, bounded, and never empty.
    #[test]
    fn a_derived_title_is_the_first_sentence_and_bounded() {
        assert_eq!(
            title_from_body("以后提交前先运行 pnpm lint。还有别的话。"),
            "以后提交前先运行 pnpm lint"
        );
        assert_eq!(title_from_body("first line\nsecond line"), "first line");
        assert_eq!(title_from_body("   "), "记忆");
        assert!(title_from_body(&"字".repeat(200)).chars().count() <= 48);
    }

    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn explicit_chinese_remember_colon_is_a_direct_write() {
        let body = parse_direct_memory_command("记住：用 pnpm").expect("direct");
        assert!(body.contains("pnpm"), "{body}");
        // It is a command, so it must NOT also be offered for approval.
        assert!(parse_inferred_preference("记住：用 pnpm").is_none());
    }

    #[test]
    fn explicit_english_remember_is_a_direct_write() {
        let body =
            parse_direct_memory_command("remember: always use WorkspaceWrite").expect("direct");
        assert!(body.contains("WorkspaceWrite"), "{body}");
    }

    /// "always use …" and "以后都…" describe a habit, so they stay proposals.
    #[test]
    fn soft_always_use_and_chinese_prefer_stay_candidates() {
        assert!(parse_direct_memory_command("always use pnpm").is_none());
        let c = parse_inferred_preference("always use pnpm").expect("inferred");
        assert_eq!(c.source, CandidateSource::SystemInferred);
        let c = parse_inferred_preference("以后都用 pnpm").expect("c");
        assert!(c.body.contains("pnpm"));
    }

    #[test]
    fn secrets_rejected() {
        assert!(parse_direct_memory_command("记住：api_key=sk-abc123secret").is_none());
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
    fn pnpm_lockfile_is_read_not_remembered() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: '9'\n").unwrap();
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
    }

    #[test]
    fn no_signal_no_package_manager() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert_eq!(package_manager_from_root(dir.path()), None);
    }

    #[test]
    fn yarn_over_npm_when_yarn_lock_present() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("yarn.lock"), "# yarn\n").unwrap();
        fs::write(dir.path().join("package-lock.json"), "{}\n").unwrap();
        assert_eq!(package_manager_from_root(dir.path()), Some("yarn"));
    }
}
