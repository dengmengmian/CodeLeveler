//! Bounded lexical recall over the topics a memory scope already lists.
//!
//! This is a read over the existing Markdown store and its generated
//! `MEMORY.md` manifest: it adds no index, no vector store and no second
//! retrieval database. A caller supplies a query (for a review, the user's
//! focus plus the source objective); this module selects a few manifest
//! entries by token overlap and returns a bounded excerpt of each topic.
//!
//! The result is historical context, never an authority. A topic is a model
//! summary unless its own text says otherwise, and the caller must present it
//! as such.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use crate::scope::MemoryScope;

/// Largest topic file this read will open. Mirrors the write limit in
/// `access_policy` (a topic that cannot be written cannot be read here).
const MAX_TOPIC_BYTES: u64 = 256 * 1024;
/// Lines of surrounding context included with the matching line.
const EXCERPT_CONTEXT_LINES: usize = 2;
/// A single query token longer than this is not a useful lexical probe.
const MAX_TOKEN_CHARS: usize = 40;

/// One bounded recall hit. `excerpt` is a prefix/window, never the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallHit {
    pub scope: MemoryScope,
    /// Manifest-relative path, for example `topics/order-settlement.md`.
    pub relative_path: String,
    pub absolute_path: PathBuf,
    pub title: String,
    pub description: String,
    pub modified: Option<SystemTime>,
    pub excerpt: String,
    /// The file is larger than the read bound.
    pub truncated: bool,
    /// Lines in the file that matched at least one query token.
    pub matched_lines: usize,
}

/// Bounds on one recall. The defaults keep a review brief small enough to
/// leave room for the reviewer's own investigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallBudget {
    pub max_topics: usize,
    pub max_excerpt_bytes: usize,
    pub max_total_bytes: usize,
}

impl Default for RecallBudget {
    fn default() -> Self {
        Self {
            max_topics: 3,
            max_excerpt_bytes: 1200,
            max_total_bytes: 6000,
        }
    }
}

/// Select and excerpt topic files that lexically overlap `query`.
///
/// A missing scope or manifest is an empty result: memory may simply have
/// nothing yet. A present but unreadable scope is an I/O error so the caller
/// can report a degraded recall instead of claiming "no relevant memory".
pub fn recall_topic_excerpts(
    global_dir: &Path,
    workspace_dir: &Path,
    query: &str,
    budget: RecallBudget,
) -> std::io::Result<Vec<RecallHit>> {
    let tokens = query_tokens(query);
    if tokens.is_empty() || budget.max_topics == 0 {
        return Ok(Vec::new());
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    for (scope, dir, rank) in [
        (MemoryScope::Workspace, workspace_dir, 0u8),
        (MemoryScope::Global, global_dir, 1u8),
    ] {
        let manifest = dir.join("MEMORY.md");
        let text = match std::fs::read_to_string(&manifest) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in manifest_entries(&text) {
            let score = score_entry(&entry, &tokens);
            if score == 0 {
                continue;
            }
            candidates.push(Candidate {
                score,
                scope,
                scope_dir: dir.to_path_buf(),
                rank,
                entry,
            });
        }
    }
    // More specific scope first on a tie; then the file path for determinism.
    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then(left.rank.cmp(&right.rank))
            .then(left.entry.relative_path.cmp(&right.entry.relative_path))
    });

    let mut hits = Vec::new();
    let mut spent = 0usize;
    for candidate in candidates {
        if hits.len() >= budget.max_topics {
            break;
        }
        if spent >= budget.max_total_bytes {
            break;
        }
        let Some(path) = safe_topic_path(&candidate.scope_dir, &candidate.entry.relative_path)
        else {
            continue;
        };
        let remaining = budget.max_total_bytes.saturating_sub(spent);
        let excerpt_budget = budget.max_excerpt_bytes.min(remaining);
        if excerpt_budget == 0 {
            break;
        }
        let Some(read) = read_excerpt(&path, &tokens, excerpt_budget) else {
            continue;
        };
        let modified = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        spent = spent.saturating_add(read.excerpt.len());
        hits.push(RecallHit {
            scope: candidate.scope,
            relative_path: candidate.entry.relative_path,
            absolute_path: path,
            title: candidate.entry.title,
            description: candidate.entry.description,
            modified,
            excerpt: read.excerpt,
            truncated: read.truncated,
            matched_lines: read.matched_lines,
        });
    }
    Ok(hits)
}

struct Candidate {
    score: usize,
    scope: MemoryScope,
    scope_dir: PathBuf,
    rank: u8,
    entry: ManifestEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestEntry {
    relative_path: String,
    title: String,
    description: String,
}

/// Parse the generated manifest's topic lines. The renderer owns this format
/// (`- **Title** — description (`topics/x.md`)`); a line that does not match
/// is skipped, never guessed.
fn manifest_entries(text: &str) -> Vec<ManifestEntry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("- **") else {
            continue;
        };
        let Some(title_end) = rest.find("**") else {
            continue;
        };
        let title = rest[..title_end].trim().to_string();
        let after = &rest[title_end + 2..];
        let Some(path_start) = after.rfind(" (`") else {
            continue;
        };
        let Some(path_end) = after.rfind("`)") else {
            continue;
        };
        // The renderer's pointer is " (`topics/x.md`)".
        let start = path_start + 3;
        if path_end <= start {
            continue;
        }
        let relative_path = after[start..path_end].trim().to_string();
        if !relative_path.starts_with("topics/") || !relative_path.ends_with(".md") {
            continue;
        }
        let description = after[..path_start]
            .trim()
            .trim_start_matches(['—', '-', ' '])
            .trim()
            .to_string();
        entries.push(ManifestEntry {
            relative_path,
            title,
            description,
        });
    }
    entries
}

fn score_entry(entry: &ManifestEntry, tokens: &[String]) -> usize {
    let title = entry.title.to_lowercase();
    let description = entry.description.to_lowercase();
    let path = entry.relative_path.to_lowercase();
    tokens.iter().fold(0usize, |score, token| {
        score
            .saturating_add(title.matches(token).count().saturating_mul(3))
            .saturating_add(description.matches(token).count().saturating_mul(2))
            .saturating_add(path.matches(token).count())
    })
}

/// Query tokens: ASCII words of at least three characters and CJK bigrams.
/// Bigrams make an unsegmented Chinese focus usable without a dictionary or a
/// second tokenizer; the manifest and topic text are the only inputs.
fn query_tokens(query: &str) -> Vec<String> {
    const ASCII_STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "from", "are", "was", "not", "but", "you",
        "all", "can", "has", "have", "its", "our", "their", "into", "when", "what", "how", "why",
        "code", "review",
    ];
    const CJK_STOPWORDS: &[&str] = &["审查", "检查", "当前", "代码", "模块", "问题", "业务"];
    let mut tokens = Vec::new();
    let mut ascii = String::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let flush_ascii = |ascii: &mut String, tokens: &mut Vec<String>| {
        let word = ascii.to_lowercase();
        if word.chars().count() >= 3
            && !ASCII_STOPWORDS.contains(&word.as_str())
            && !tokens.iter().any(|existing| existing == &word)
        {
            tokens.push(word);
        }
        ascii.clear();
    };
    let flush_cjk = |run: &mut Vec<char>, tokens: &mut Vec<String>| {
        if run.len() == 1 {
            let token: String = run.iter().collect();
            if !tokens.contains(&token) {
                tokens.push(token);
            }
        } else {
            for pair in run.windows(2) {
                let token: String = pair.iter().collect();
                if !CJK_STOPWORDS.contains(&token.as_str()) && !tokens.contains(&token) {
                    tokens.push(token);
                }
            }
        }
        run.clear();
    };
    for character in query.chars() {
        if is_cjk(character) {
            flush_ascii(&mut ascii, &mut tokens);
            if cjk_run.len() >= MAX_TOKEN_CHARS {
                flush_cjk(&mut cjk_run, &mut tokens);
            }
            cjk_run.push(character);
        } else if character.is_alphanumeric() || character == '_' {
            flush_cjk(&mut cjk_run, &mut tokens);
            if ascii.chars().count() < MAX_TOKEN_CHARS {
                ascii.push(character);
            }
        } else {
            flush_ascii(&mut ascii, &mut tokens);
            flush_cjk(&mut cjk_run, &mut tokens);
        }
    }
    flush_ascii(&mut ascii, &mut tokens);
    flush_cjk(&mut cjk_run, &mut tokens);
    tokens
}

fn is_cjk(character: char) -> bool {
    matches!(character,
        '\u{3400}'..='\u{4dbf}'
        | '\u{4e00}'..='\u{9fff}'
        | '\u{f900}'..='\u{faff}'
        | '\u{3040}'..='\u{30ff}'
        | '\u{ac00}'..='\u{d7af}')
}

struct ExcerptRead {
    excerpt: String,
    truncated: bool,
    matched_lines: usize,
}

/// Read a bounded window around the first token-bearing line. If the entry's
/// title matched but the body does not, return the first non-heading block so
/// the caller still sees what the topic says.
fn read_excerpt(path: &Path, tokens: &[String], max_bytes: usize) -> Option<ExcerptRead> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let truncated = bytes.len() as u64 > MAX_TOPIC_BYTES;
    let bounded = if truncated {
        bytes
            .get(..MAX_TOPIC_BYTES as usize)
            .unwrap_or(bytes.as_slice())
    } else {
        bytes.as_slice()
    };
    let text = String::from_utf8_lossy(bounded);
    let lines: Vec<&str> = text.lines().collect();
    let matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line_matches(line, tokens))
        .map(|(index, _)| index)
        .collect();
    let (start, end) = match matches.first() {
        Some(first) => (
            first.saturating_sub(EXCERPT_CONTEXT_LINES),
            (first + EXCERPT_CONTEXT_LINES + 1).min(lines.len()),
        ),
        None => {
            let start = lines
                .iter()
                .position(|line| {
                    let line = line.trim();
                    !line.is_empty() && !line.starts_with('#')
                })
                .unwrap_or(0);
            (start, (start + EXCERPT_CONTEXT_LINES + 2).min(lines.len()))
        }
    };
    let mut excerpt = lines
        .get(start..end)
        .map(|slice| slice.join("\n"))
        .unwrap_or_default();
    let mut byte_truncated = false;
    if excerpt.len() > max_bytes {
        let cut = floor_char_boundary(&excerpt, max_bytes);
        excerpt.truncate(cut);
        excerpt.push('…');
        byte_truncated = true;
    }
    Some(ExcerptRead {
        excerpt,
        truncated: truncated || byte_truncated,
        matched_lines: matches.len(),
    })
}

fn line_matches(line: &str, tokens: &[String]) -> bool {
    let lower = line.to_lowercase();
    tokens.iter().any(|token| lower.contains(token))
}

fn floor_char_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// Resolve a manifest-relative topic path without allowing traversal, an
/// absolute path, a non-topic path, a symlink, or a scope escape.
fn safe_topic_path(scope_dir: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    if relative.is_absolute() {
        return None;
    }
    let mut components = relative.components();
    match components.next()? {
        Component::Normal(first) if first == "topics" => {}
        _ => return None,
    }
    for component in components {
        match component {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return None;
            }
        }
    }
    if relative.extension().and_then(|value| value.to_str()) != Some("md") {
        return None;
    }
    let path = scope_dir.join(relative);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let root = dunce::canonicalize(scope_dir).ok()?;
    let canonical = dunce::canonicalize(&path).ok()?;
    canonical.starts_with(&root).then_some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(root: &Path, name: &str, entries: &[(&str, &str, &str, &str)]) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("topics")).unwrap();
        let mut manifest = String::from("# Workspace memory index\n\n## Topics\n");
        for (title, description, path, body) in entries {
            manifest.push_str(&format!("\n- **{title}** — {description} (`{path}`)\n"));
            std::fs::write(dir.join(path), body).unwrap();
        }
        std::fs::write(dir.join("MEMORY.md"), manifest).unwrap();
        dir
    }

    #[test]
    fn recall_selects_the_topic_that_matches_the_focus_and_returns_its_evidence() {
        let root = tempfile::tempdir().unwrap();
        let workspace = scope(
            root.path(),
            "workspaces/order",
            &[
                (
                    "订单结算规则",
                    "终止订单与结算的边界",
                    "topics/order-settlement.md",
                    "## 终止订单\n\n终止事实必须保留；终止后的订单不得进入结算。\n\n## 其他\n\n无关内容。\n",
                ),
                (
                    "构建配置",
                    "CI and release",
                    "topics/build.md",
                    "## Build\n\ncargo build.\n",
                ),
            ],
        );
        let hits = recall_topic_excerpts(
            &root.path().join("global"),
            &workspace,
            "审查当前订单结算模块的代码及业务正确性",
            RecallBudget::default(),
        )
        .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        let hit = hits.first().expect("one hit");
        assert_eq!(hit.relative_path, "topics/order-settlement.md");
        assert_eq!(hit.scope, MemoryScope::Workspace);
        assert!(hit.excerpt.contains("终止后的订单不得进入结算"));
        assert!(hit.matched_lines > 0);
    }

    #[test]
    fn a_query_with_no_lexical_overlap_returns_nothing() {
        let root = tempfile::tempdir().unwrap();
        let workspace = scope(
            root.path(),
            "workspaces/order",
            &[(
                "构建配置",
                "CI and release",
                "topics/build.md",
                "## Build\n\ncargo build.\n",
            )],
        );
        let hits = recall_topic_excerpts(
            &root.path().join("global"),
            &workspace,
            "订单结算",
            RecallBudget::default(),
        )
        .unwrap();
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn the_topic_count_and_excerpt_bytes_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        let long = "订单结算".repeat(200);
        let workspace = scope(
            root.path(),
            "workspaces/order",
            &[
                ("结算一", "订单结算", "topics/a.md", &long),
                ("结算二", "订单结算", "topics/b.md", &long),
                ("结算三", "订单结算", "topics/c.md", &long),
            ],
        );
        let hits = recall_topic_excerpts(
            &root.path().join("global"),
            &workspace,
            "订单结算",
            RecallBudget {
                max_topics: 2,
                max_excerpt_bytes: 64,
                max_total_bytes: 100,
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        for hit in &hits {
            assert!(hit.excerpt.len() <= 64 + 4, "{}", hit.excerpt.len());
        }
        assert!(hits.iter().all(|hit| hit.truncated));
    }

    #[test]
    fn traversal_and_symlinked_topics_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside.md");
        std::fs::write(&outside, "订单结算 secret\n").unwrap();
        let workspace = scope(
            root.path(),
            "workspaces/order",
            &[("结算", "订单结算", "topics/a.md", "订单结算 body\n")],
        );
        // A manifest line can be edited by hand. Neither spelling may escape.
        let manifest = format!(
            "{}\n- **越界** — 订单结算 (`topics/../outside.md`)\n- **链接** — 订单结算 (`topics/link.md`)\n",
            std::fs::read_to_string(workspace.join("MEMORY.md")).unwrap()
        );
        std::fs::write(workspace.join("MEMORY.md"), manifest).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, workspace.join("topics/link.md")).unwrap();
        let hits = recall_topic_excerpts(
            &root.path().join("global"),
            &workspace,
            "订单结算",
            RecallBudget::default(),
        )
        .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits.first().expect("one hit").relative_path, "topics/a.md");
    }

    #[test]
    fn a_missing_scope_is_an_empty_recall_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        let hits = recall_topic_excerpts(
            &root.path().join("global"),
            &root.path().join("workspaces/missing"),
            "订单结算",
            RecallBudget::default(),
        )
        .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn the_workspace_scope_wins_a_tie_against_global() {
        let root = tempfile::tempdir().unwrap();
        let global = scope(
            root.path(),
            "global",
            &[("结算", "订单结算", "topics/a.md", "订单结算 global\n")],
        );
        let workspace = scope(
            root.path(),
            "workspaces/order",
            &[("结算", "订单结算", "topics/a.md", "订单结算 workspace\n")],
        );
        let hits = recall_topic_excerpts(&global, &workspace, "订单结算", RecallBudget::default())
            .unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(
            hits.first().expect("first hit").scope,
            MemoryScope::Workspace
        );
    }
}
