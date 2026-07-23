//! `glob` — find files whose path matches a glob pattern (e.g. `**/*_test.go`).
//! Complements `repository_search` (case-insensitive substring) and `grep`
//! (file contents). Lists tracked and untracked-but-not-ignored files via git
//! (falling back to a filesystem walk), then matches each path against the pattern.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::{ProcessRequest, RiskLevel};

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_MAX: usize = 100;
const IGNORED: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "vendor",
    ".leveler",
];

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Glob pattern matched against workspace-relative file paths. `*` matches
    /// within one path segment, `**` matches across directories, `?` matches a
    /// single character. E.g. `**/*_test.go`, `src/**/*.rs`, `*.md`.
    pattern: String,
    /// Maximum results. Defaults to 100.
    #[serde(default)]
    max_results: Option<usize>,
}

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by a case-sensitive glob pattern (`*` within a path segment, \
         `**` across directories, `?` a single character), e.g. `**/*_test.go` or \
         `src/**/*.rs`. Use this when you know the shape of the name. If you only \
         have a rough, possibly mis-cased substring, use `repository_search` \
         instead; use `grep` to search file contents."
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
        let max = input.max_results.unwrap_or(DEFAULT_MAX);

        let files = list_files(&context, &cancellation).await;
        let mut matches: Vec<String> = files
            .into_iter()
            .filter(|path| glob_match(&input.pattern, path))
            .collect();
        matches.sort();
        matches.dedup();
        let total = matches.len();
        matches.truncate(max);

        if matches.is_empty() {
            return Ok(ToolOutput::ok("(no matching files)\n"));
        }
        let mut body = matches.join("\n");
        body.push('\n');
        if total > max {
            body.push_str(&format!(
                "… [showing {max} of {total} matches; raise max_results or \
                 narrow the pattern]\n"
            ));
        }
        Ok(ToolOutput::ok(body))
    }
}

/// Match a workspace-relative path against a glob pattern, segment by segment.
/// `**` spans zero or more directory segments; `*` and `?` stay within a single
/// segment (they never cross `/`).
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let seg: Vec<&str> = path.split('/').collect();
    segments_match(&pat, &seg)
}

fn segments_match(pat: &[&str], seg: &[&str]) -> bool {
    match pat.first() {
        None => seg.is_empty(),
        Some(&"**") => {
            // `**` consumes zero or more whole segments.
            (0..=seg.len()).any(|k| segments_match(&pat[1..], &seg[k..]))
        }
        Some(p) => {
            !seg.is_empty() && one_segment(p, seg[0]) && segments_match(&pat[1..], &seg[1..])
        }
    }
}

/// Classic `*`/`?` glob within a single segment (no `/`), by backtracking.
fn one_segment(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // Backtrack point for the most recent `*`.
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// List candidate files via git (tracked **and** untracked-but-not-ignored, so
/// a file the agent just wrote is found), falling back to a filesystem walk when
/// not in a git repo. `--cached --others --exclude-standard` mirrors what a
/// gitignore-aware `fd` would return.
async fn list_files(context: &ToolContext, cancellation: &CancellationToken) -> Vec<String> {
    let mut request = ProcessRequest::new(
        "git",
        vec![
            "ls-files".into(),
            "--cached".into(),
            "--others".into(),
            "--exclude-standard".into(),
        ],
        context.workspace.root().to_path_buf(),
    );
    request.timeout = Duration::from_secs(30);
    if let Ok(output) = context
        .runner
        .run(request, cancellation.child_token())
        .await
        && output.success()
        && !output.stdout.trim().is_empty()
    {
        return output.stdout.lines().map(str::to_string).collect();
    }
    let mut files = Vec::new();
    walk(
        context.workspace.root(),
        context.workspace.root(),
        &mut files,
    );
    files
}

fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    if out.len() > 5000 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globstar_spans_directories_but_star_stays_in_segment() {
        assert!(glob_match("**/*_test.go", "internal/model/user_test.go"));
        assert!(glob_match("**/*_test.go", "user_test.go")); // ** matches zero dirs
        assert!(!glob_match("**/*_test.go", "internal/model/user.go"));
        assert!(glob_match("src/**/*.rs", "src/a/b/c.rs"));
        assert!(glob_match("src/**/*.rs", "src/lib.rs"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/a/lib.rs")); // * does not cross /
        assert!(glob_match("*.go", "main.go"));
        assert!(!glob_match("*.go", "internal/model/user.go"));
        assert!(glob_match("a?c.txt", "abc.txt"));
        assert!(!glob_match("a?c.txt", "ac.txt"));
    }

    #[tokio::test]
    async fn execute_returns_only_pattern_matches() {
        let dir =
            std::env::temp_dir().join(format!("leveler-glob-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("internal/model")).unwrap();
        std::fs::create_dir_all(dir.join("cmd")).unwrap();
        std::fs::write(dir.join("internal/model/user_test.go"), "").unwrap();
        std::fs::write(dir.join("internal/model/user.go"), "").unwrap();
        std::fs::write(dir.join("cmd/main.go"), "").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = GlobTool
            .execute(
                serde_json::json!({ "pattern": "**/*_test.go" }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("user_test.go"), "got: {}", out.content);
        assert!(!out.content.contains("user.go"), "got: {}", out.content);
        assert!(!out.content.contains("main.go"), "got: {}", out.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// In a git repo, `git ls-files` alone only lists *tracked* files, so a file
    /// the agent just created but hasn't `git add`ed is invisible to glob — it
    /// fails to find its own new file. The listing must include untracked (but
    /// not gitignored) files.
    #[tokio::test]
    async fn finds_untracked_new_files_in_a_git_repo() {
        let dir = std::env::temp_dir()
            .join(format!("leveler-glob-untracked-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("src/tracked.rs"), "").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        // A brand-new file the model wrote this turn — never git-added, not ignored.
        std::fs::write(dir.join("src/fresh.rs"), "").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.join("src/ignored.rs"), "").unwrap();

        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = GlobTool
            .execute(
                serde_json::json!({ "pattern": "src/*.rs" }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("tracked.rs"), "tracked: {}", out.content);
        assert!(
            out.content.contains("fresh.rs"),
            "must find the just-created untracked file: {}",
            out.content
        );
        assert!(
            !out.content.contains("ignored.rs"),
            "gitignored file must stay excluded: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn capped_results_report_the_total() {
        // The total is known before truncation — the marker must state it.
        let dir =
            std::env::temp_dir().join(format!("leveler-glob-cap-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.go"), "").unwrap();
        std::fs::write(dir.join("b.go"), "").unwrap();
        std::fs::write(dir.join("c.go"), "").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = GlobTool
            .execute(
                serde_json::json!({ "pattern": "*.go", "max_results": 2 }),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            out.content.contains("2 of 3"),
            "marker must report shown/total: {}",
            out.content
        );
        assert!(
            out.content.contains("max_results"),
            "marker must name the knob: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
