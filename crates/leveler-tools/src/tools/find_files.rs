//! `find_files` — locate files by glob shape or forgiving path substring.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{ProcessRequest, RiskLevel};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_MAX: usize = 100;
const HARD_MAX: usize = 1000;
const FALLBACK_CANDIDATE_LIMIT: usize = 5000;
const IGNORED: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "vendor",
    ".leveler",
];

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum MatchMode {
    /// Infer glob mode when `pattern` contains `*` or `?`; substring otherwise.
    #[default]
    Auto,
    /// Case-insensitive substring of the workspace-relative path.
    Substring,
    /// Case-sensitive `*`, `**`, and `?` path glob.
    Glob,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// File-name/path pattern. Plain text is a case-insensitive substring;
    /// `*`, `**`, and `?` automatically select glob matching.
    pattern: String,
    /// Override automatic matching: auto (default), substring, or glob.
    #[serde(default)]
    mode: MatchMode,
    /// Optional directory to search below. Defaults to the workspace root.
    #[serde(default)]
    path: Option<String>,
    /// Optional extension filter, with or without a leading dot.
    #[serde(default)]
    extension: Option<String>,
    /// Maximum results (default 100, hard cap 1000).
    #[serde(default)]
    max_results: Option<usize>,
}

pub struct FindFilesTool;

#[async_trait]
impl Tool for FindFilesTool {
    fn name(&self) -> &'static str {
        "find_files"
    }

    fn description(&self) -> &'static str {
        "Find files by path/name. `pattern` is a forgiving case-insensitive \
         substring by default; patterns containing `*`, `**`, or `?` are treated \
         as case-sensitive globs (or set mode explicitly). Optional `path`, \
         `extension`, and `max_results` narrow the search. Use `grep` for file \
         contents and `list_files` to inspect a directory tree."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        if input.pattern.is_empty() {
            return Ok(ToolOutput::error("`pattern` must not be empty"));
        }
        let base_label = input.path.as_deref().unwrap_or(".");
        let base = context.workspace.resolve_read(base_label)?;
        if !base.is_dir() {
            return Ok(ToolOutput::error(crate::recoverable::path_not_directory(
                base_label,
            )));
        }
        let mode = match input.mode {
            MatchMode::Auto if input.pattern.contains(['*', '?']) => MatchMode::Glob,
            MatchMode::Auto => MatchMode::Substring,
            mode => mode,
        };
        let extension = input
            .extension
            .as_deref()
            .map(|extension| extension.trim_start_matches('.'))
            .filter(|extension| !extension.is_empty());
        let max = input.max_results.unwrap_or(DEFAULT_MAX).clamp(1, HARD_MAX);
        let candidates = enumerate_candidates(&context, &base, &cancellation).await;

        let query_lower =
            matches!(mode, MatchMode::Substring).then(|| input.pattern.to_lowercase());
        let mut matches: Vec<String> = candidates
            .paths
            .into_iter()
            .filter(|candidate| {
                let pattern_matches = match mode {
                    MatchMode::Glob => glob_match(&input.pattern, candidate),
                    MatchMode::Substring => candidate
                        .to_lowercase()
                        .contains(query_lower.as_deref().unwrap_or_default()),
                    MatchMode::Auto => unreachable!("auto mode is resolved above"),
                };
                pattern_matches
                    && extension
                        .map(|extension| {
                            std::path::Path::new(candidate)
                                .extension()
                                .and_then(|value| value.to_str())
                                .is_some_and(|value| value.eq_ignore_ascii_case(extension))
                        })
                        .unwrap_or(true)
            })
            .collect();
        matches.sort();
        matches.dedup();
        let total = matches.len();
        matches.truncate(max);

        let mut body = if matches.is_empty() {
            "(no matching files)\n".to_string()
        } else {
            format!("{}\n", matches.join("\n"))
        };
        if total > max {
            body.push_str(&format!(
                "… [showing {max} of {total} matches; raise max_results or narrow \
                 pattern/path]\n"
            ));
        }
        if candidates.truncated {
            body.push_str(&format!(
                "… [candidate scan was capped at {FALLBACK_CANDIDATE_LIMIT} entries; \
                 results may be incomplete — narrow `path`]\n"
            ));
        }
        Ok(ToolOutput::ok(body))
    }
}

struct Candidates {
    paths: Vec<String>,
    truncated: bool,
}

/// One candidate enumerator shared by substring and glob matching.
async fn enumerate_candidates(
    context: &ToolContext,
    base: &std::path::Path,
    cancellation: &CancellationToken,
) -> Candidates {
    let mut request = ProcessRequest::new(
        "git",
        vec![
            "ls-files".into(),
            "--cached".into(),
            "--others".into(),
            "--exclude-standard".into(),
        ],
        base.to_path_buf(),
    );
    request.timeout = Duration::from_secs(30);
    if let Ok(output) = context
        .runner
        .run(request, cancellation.child_token())
        .await
        && output.success()
    {
        return Candidates {
            paths: output.stdout.lines().map(str::to_string).collect(),
            truncated: output.truncated,
        };
    }

    let mut candidates = Candidates {
        paths: Vec::new(),
        truncated: false,
    };
    walk(base, base, &mut candidates);
    candidates
}

fn walk(root: &std::path::Path, dir: &std::path::Path, candidates: &mut Candidates) {
    if candidates.paths.len() >= FALLBACK_CANDIDATE_LIMIT {
        candidates.truncated = true;
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        if candidates.paths.len() >= FALLBACK_CANDIDATE_LIMIT {
            candidates.truncated = true;
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, candidates);
        } else if let Ok(relative) = path.strip_prefix(root) {
            candidates
                .paths
                .push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let segments: Vec<&str> = path.split('/').collect();
    segments_match(&pattern, &segments, 0, 0, &mut HashMap::new())
}

fn segments_match(
    pattern: &[&str],
    segments: &[&str],
    pattern_index: usize,
    segment_index: usize,
    memo: &mut HashMap<(usize, usize), bool>,
) -> bool {
    if let Some(&cached) = memo.get(&(pattern_index, segment_index)) {
        return cached;
    }
    let matched = match pattern.get(pattern_index) {
        None => segment_index == segments.len(),
        Some(&"**") => {
            segments_match(pattern, segments, pattern_index + 1, segment_index, memo)
                || (segment_index < segments.len()
                    && segments_match(pattern, segments, pattern_index, segment_index + 1, memo))
        }
        Some(part) => {
            segment_index < segments.len()
                && one_segment(part, segments[segment_index])
                && segments_match(
                    pattern,
                    segments,
                    pattern_index + 1,
                    segment_index + 1,
                    memo,
                )
        }
    };
    memo.insert((pattern_index, segment_index), matched);
    matched
}

fn one_segment(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pattern_index, mut text_index) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            mark = text_index;
            pattern_index += 1;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            mark += 1;
            text_index = mark;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(
            leveler_execution::Workspace::new(dir).unwrap(),
            leveler_execution::PermissionProfile::RequestApproval,
        )
    }

    #[test]
    fn globstar_is_memoized_and_matches_across_directories() {
        assert!(glob_match("**/*_test.go", "internal/model/user_test.go"));
        assert!(glob_match("**/*_test.go", "user_test.go"));
        assert!(glob_match("src/**/*.rs", "src/lib.rs"));
        assert!(glob_match("src/**/*.rs", "src/a/b/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/a/lib.rs"));

        let adversarial = format!("{}/end.rs", "a/".repeat(200));
        assert!(glob_match("**/**/**/end.rs", &adversarial));
    }

    #[tokio::test]
    async fn auto_mode_supports_substring_and_glob_through_one_tool() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-find-files-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(dir.join("internal/model")).unwrap();
        std::fs::write(dir.join("internal/model/Order_Service.rs"), "").unwrap();
        std::fs::write(dir.join("internal/model/user_test.go"), "").unwrap();

        let substring = FindFilesTool
            .execute(
                serde_json::json!({"pattern": "order_service", "extension": "rs"}),
                context(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(substring.content.contains("Order_Service.rs"));

        let glob = FindFilesTool
            .execute(
                serde_json::json!({"pattern": "**/*_test.go"}),
                context(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(glob.content.contains("user_test.go"));
        assert!(!glob.content.contains("Order_Service.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn fallback_cap_is_reported_instead_of_silent() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-find-files-cap-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for index in 0..=FALLBACK_CANDIDATE_LIMIT {
            std::fs::write(dir.join(format!("file-{index:05}.txt")), "").unwrap();
        }
        let out = FindFilesTool
            .execute(
                serde_json::json!({"pattern": "file-", "max_results": 1}),
                context(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("candidate scan was capped"),
            "{}",
            out.content
        );
        assert!(out.content.contains("narrow `path`"), "{}", out.content);
        std::fs::remove_dir_all(&dir).ok();
    }
}
