//! `git_history` — structural, read-only queries over a repository's object
//! database: a commit list, one commit's change, a file at a revision, a
//! revision-scoped search/listing, and blame.
//!
//! This exists because the ordinary file tools read the WORKING TREE. When a
//! review targets a fixed commit or a branch range, those tools cannot answer
//! for the reviewed object, and presenting worktree content as the commit's
//! content would be false evidence. Every action here resolves the requested
//! revision to a commit SHA first and then reads that object; a revision that
//! cannot be resolved is an error, never a quiet fallback to HEAD.
//!
//! The adapter is thin: `leveler-vcs` owns how `git` is invoked, and this tool
//! owns only argument validation and rendering.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_vcs::GitWorkflow;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_REV_BYTES: usize = 200;
const MAX_PATH_BYTES: usize = 1000;
const MAX_PATTERN_BYTES: usize = 1000;
const DEFAULT_LOG_COUNT: u32 = 20;
const MAX_LOG_COUNT: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Action {
    /// List commits, optionally restricted to a path.
    Log,
    /// One commit's metadata, stat and patch, optionally restricted to a path.
    Show,
    /// The exact content of one file at a revision.
    File,
    /// Search a revision's tree with `git grep`.
    Search,
    /// List the files in a revision's tree.
    List,
    /// Line-attributed history for one file at a revision.
    Blame,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Which historical query to run.
    action: Action,
    /// Commit-ish to resolve (branch, tag, `HEAD~3`, SHA). Default `HEAD`.
    #[serde(default)]
    rev: Option<String>,
    /// Repository-relative path. Required by `file` and `blame`; optional
    /// filter for `log`, `show`, `search` and `list`.
    #[serde(default)]
    path: Option<String>,
    /// Search expression for `search` (git basic regular expression).
    #[serde(default)]
    pattern: Option<String>,
    /// Maximum commits for `log` (1–200, default 20).
    #[serde(default)]
    max_count: Option<u32>,
    /// First line for `blame` (1-based, inclusive). With `line_end`, limits
    /// the attributed range; omit both for the whole file.
    #[serde(default)]
    line_start: Option<u32>,
    /// Last line for `blame` (1-based, inclusive).
    #[serde(default)]
    line_end: Option<u32>,
}

pub struct GitHistoryTool {
    /// When set, only this tip and its ancestors are readable. A fixed-object
    /// review uses it so a later branch (including the commit that fixed the
    /// reviewed defect) cannot leak into the evidence set.
    scope_tip: Option<String>,
}

impl GitHistoryTool {
    pub fn new() -> Self {
        Self { scope_tip: None }
    }

    /// Bind the tool to one reviewed tip. `tip` is resolved to a commit when
    /// the tool runs; anything outside its ancestry is refused.
    pub fn scoped_to(tip: impl Into<String>) -> Self {
        Self {
            scope_tip: Some(tip.into()),
        }
    }
}

impl Default for GitHistoryTool {
    fn default() -> Self {
        Self::new()
    }
}

fn vcs(context: &ToolContext) -> Result<GitWorkflow, ToolError> {
    Ok(GitWorkflow::with_runner(
        context.require_workspace()?.root(),
        context.execution.runner.clone(),
        context.execution.environment.clone(),
    ))
}

fn valid_rev(raw: &str) -> Result<String, String> {
    let rev = raw.trim();
    if rev.is_empty()
        || rev.len() > MAX_REV_BYTES
        || rev.starts_with('-')
        || rev.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(format!("invalid revision `{raw}`"));
    }
    Ok(rev.to_string())
}

/// A repository-relative path usable in a git pathspec and in the
/// `<rev>:<path>` form. Absolute paths, parent traversal, option-shaped
/// values and pathspec magic are refused rather than reinterpreted.
fn valid_path(raw: &str) -> Result<String, String> {
    let path = raw.trim();
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.starts_with('-')
        || path.starts_with(':')
        || path.contains('\0')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| part == "..")
        || path.contains(':')
    {
        return Err(format!("invalid repository path `{raw}`"));
    }
    let normalized = path.trim_start_matches("./").trim_end_matches('/');
    if normalized.is_empty() {
        return Err(format!("invalid repository path `{raw}`"));
    }
    Ok(normalized.to_string())
}

fn invalid(message: impl Into<String>) -> ToolOutput {
    ToolOutput::error(format!("{}\n", message.into()))
}

fn render(output: leveler_execution::ProcessOutput) -> ToolOutput {
    if !output.success() {
        return ToolOutput::error(format!(
            "git failed (exit {:?}):\n{}",
            output.exit_code,
            format!("{}{}", output.stdout, output.stderr).trim()
        ));
    }
    let mut body = if output.stdout.trim().is_empty() {
        "(no output)\n".to_string()
    } else {
        output.stdout
    };
    if output.truncated {
        body.push_str(
            "\n[note] output was truncated by the process output cap; narrow the query \
             (a path, a line range, or a smaller max_count) for the rest.\n",
        );
    }
    ToolOutput::ok(body)
}

fn render_part(output: &leveler_execution::ProcessOutput) -> Result<String, ToolOutput> {
    if !output.success() {
        return Err(ToolOutput::error(format!(
            "git failed (exit {:?}):\n{}",
            output.exit_code,
            format!("{}{}", output.stdout, output.stderr).trim()
        )));
    }
    Ok(output.stdout.clone())
}

/// `git show` on a merge prints a combined diff, which is empty for a clean
/// merge even though the commit carries a whole branch's changes. Render the
/// same first-parent diff the review brief uses, and say which parent was
/// chosen. A merge is still readable by `file`/`search`/`list`; only the
/// `show` rendering needs this special case.
async fn show_merge(
    workflow: &GitWorkflow,
    sha: &str,
    path: Option<&str>,
    cancellation: CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let header = workflow
        .inspect_history(
            &["--no-pager", "show", "--no-patch", "--format=fuller", sha],
            cancellation.clone(),
        )
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    let mut args = vec![
        "--no-pager".to_string(),
        "diff".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        "--stat".to_string(),
        "--patch".to_string(),
        format!("{sha}^1"),
        sha.to_string(),
    ];
    if let Some(path) = path {
        args.push("--".to_string());
        args.push(path.to_string());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let diff = workflow
        .inspect_history(&argv, cancellation)
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    let mut body = match render_part(&header) {
        Ok(text) => text,
        Err(output) => return Ok(output),
    };
    body.push_str(&format!(
        "\n[note] merge commit: this diff is against the first parent ({sha}^1), not a combined diff.\n\n"
    ));
    body.push_str(&match render_part(&diff) {
        Ok(text) => text,
        Err(output) => return Ok(output),
    });
    if header.truncated || diff.truncated {
        body.push_str(
            "\n[note] output was truncated by the process output cap; narrow the query (a path) for the rest.\n",
        );
    }
    Ok(ToolOutput::ok(body))
}

#[async_trait]
impl Tool for GitHistoryTool {
    fn name(&self) -> &'static str {
        "git_history"
    }

    fn description(&self) -> &'static str {
        "Read a repository's committed history and object database without touching the \
         working tree. Actions: `log` (commits, optional path), `show` (one commit's \
         metadata/stat/patch, optional path), `file` (exact file content at a revision), \
         `search` (git grep inside a revision's tree), `list` (files in a revision's tree), \
         `blame` (line attribution for a file). The revision is resolved to a commit SHA \
         first; an unknown revision fails instead of falling back to HEAD. Use this for \
         evidence about a commit or branch review, where the ordinary file tools read the \
         current worktree."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let workflow = vcs(&context)?;
        let invalid_argument = |message: String| ToolError::InvalidArguments {
            tool: self.name().to_string(),
            message,
        };
        let default_rev = self.scope_tip.as_deref().unwrap_or("HEAD");
        let rev =
            valid_rev(input.rev.as_deref().unwrap_or(default_rev)).map_err(invalid_argument)?;
        let path = match input.path.as_deref() {
            Some(raw) => Some(
                valid_path(raw).map_err(|message| ToolError::InvalidArguments {
                    tool: self.name().to_string(),
                    message,
                })?,
            ),
            None => None,
        };
        // A path that names a credential/trust-gated file is refused by the
        // same workspace authority file reads use. The object may not exist in
        // the worktree (it can be deleted at this revision), so the check is
        // applied to the lexical path, not to an existing file.
        if let Some(path) = &path {
            context.execution_workspace()?.resolve_for_read(path)?;
        }

        // Resolve ONCE, then use the SHA. `--end-of-options` keeps a
        // revision-shaped option from being read as one, and `^{commit}`
        // refuses a tree/blob instead of silently accepting it.
        let verify = workflow
            .inspect_history(
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    &format!("{rev}^{{commit}}"),
                ],
                cancellation.clone(),
            )
            .await
            .map_err(|error| ToolError::Io(error.to_string()))?;
        if !verify.success() {
            return Ok(invalid(format!(
                "cannot resolve revision `{rev}` to a commit: {}",
                format!("{}{}", verify.stdout, verify.stderr).trim()
            )));
        }
        let sha = verify.stdout.trim().to_string();
        if sha.is_empty() {
            return Ok(invalid(format!("revision `{rev}` resolved to nothing")));
        }
        // A fixed-object review may read the reviewed tip and its history.
        // A sibling/later ref (for example the branch that fixed this defect)
        // is outside the object and must not become evidence.
        if let Some(tip) = &self.scope_tip {
            let tip_verify = workflow
                .inspect_history(
                    &[
                        "rev-parse",
                        "--verify",
                        "--end-of-options",
                        &format!("{tip}^{{commit}}"),
                    ],
                    cancellation.clone(),
                )
                .await
                .map_err(|error| ToolError::Io(error.to_string()))?;
            if !tip_verify.success() {
                return Ok(invalid(format!(
                    "the review's object revision `{tip}` cannot be resolved"
                )));
            }
            let tip_sha = tip_verify.stdout.trim().to_string();
            let ancestry = workflow
                .inspect_history(
                    &["merge-base", "--is-ancestor", &sha, &tip_sha],
                    cancellation.clone(),
                )
                .await
                .map_err(|error| ToolError::Io(error.to_string()))?;
            match ancestry.exit_code {
                Some(0) => {}
                Some(1) => {
                    return Ok(invalid(format!(
                        "revision `{rev}` is outside the reviewed object's ancestry (`{tip}`); \
                         a review cannot use a later or sibling commit as evidence for it"
                    )));
                }
                _ => {
                    return Err(ToolError::Io(format!(
                        "could not check whether `{rev}` belongs to the reviewed object: {}",
                        format!("{}{}", ancestry.stdout, ancestry.stderr).trim()
                    )));
                }
            }
        }

        let args: Vec<String> = match input.action {
            Action::Log => {
                let count = input
                    .max_count
                    .unwrap_or(DEFAULT_LOG_COUNT)
                    .clamp(1, MAX_LOG_COUNT);
                let mut args = vec![
                    "--no-pager".into(),
                    "log".into(),
                    "--no-show-signature".into(),
                    "--format=%H%x09%aI%x09%an%x09%s".into(),
                    "-n".into(),
                    count.to_string(),
                    sha.clone(),
                ];
                if let Some(path) = &path {
                    args.push("--".into());
                    args.push(path.clone());
                }
                args
            }
            Action::Show => {
                let parents = workflow
                    .inspect_history(
                        &["rev-list", "--parents", "-n", "1", &sha],
                        cancellation.clone(),
                    )
                    .await
                    .map_err(|error| ToolError::Io(error.to_string()))?;
                if parents.success() && parents.stdout.split_whitespace().count() > 2 {
                    return show_merge(&workflow, &sha, path.as_deref(), cancellation).await;
                }
                let mut args = vec![
                    "--no-pager".into(),
                    "show".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                    "--format=fuller".into(),
                    "--stat".into(),
                    "--patch".into(),
                    sha.clone(),
                ];
                if let Some(path) = &path {
                    args.push("--".into());
                    args.push(path.clone());
                }
                args
            }
            Action::File => {
                let Some(path) = &path else {
                    return Ok(invalid("action `file` requires `path`"));
                };
                vec!["--no-pager".into(), "show".into(), format!("{sha}:{path}")]
            }
            Action::Search => {
                let Some(pattern) = input
                    .pattern
                    .as_deref()
                    .map(str::trim)
                    .filter(|pattern| !pattern.is_empty())
                else {
                    return Ok(invalid("action `search` requires `pattern`"));
                };
                if pattern.len() > MAX_PATTERN_BYTES || pattern.contains('\0') {
                    return Ok(invalid("`pattern` is too long or contains a NUL byte"));
                }
                let mut args = vec![
                    "--no-pager".into(),
                    "grep".into(),
                    "-n".into(),
                    "-I".into(),
                    "-e".into(),
                    pattern.to_string(),
                    sha.clone(),
                ];
                if let Some(path) = &path {
                    args.push("--".into());
                    args.push(path.clone());
                }
                args
            }
            Action::List => {
                let mut args = vec![
                    "--no-pager".into(),
                    "ls-tree".into(),
                    "-r".into(),
                    "--name-only".into(),
                    sha.clone(),
                ];
                if let Some(path) = &path {
                    args.push("--".into());
                    args.push(path.clone());
                }
                args
            }
            Action::Blame => {
                let Some(path) = &path else {
                    return Ok(invalid("action `blame` requires `path`"));
                };
                let mut args = vec!["--no-pager".into(), "blame".into(), "--abbrev=12".into()];
                match (input.line_start, input.line_end) {
                    (Some(start), Some(end)) => {
                        if start == 0 || end < start {
                            return Ok(invalid(
                                "`line_start` must be >= 1 and `line_end` must be >= `line_start`",
                            ));
                        }
                        args.push("-L".into());
                        args.push(format!("{start},{end}"));
                    }
                    (Some(_), None) | (None, Some(_)) => {
                        return Ok(invalid(
                            "blame line ranges require both `line_start` and `line_end`",
                        ));
                    }
                    (None, None) => {}
                }
                args.push(sha.clone());
                args.push("--".into());
                args.push(path.clone());
                args
            }
        };

        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = workflow
            .inspect_history(&argv, cancellation)
            .await
            .map_err(|error| ToolError::Io(error.to_string()))?;
        Ok(render(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    struct Fixture {
        dir: std::path::PathBuf,
        first: String,
        second: String,
    }

    fn fixture() -> Fixture {
        let dir = std::env::temp_dir().join(format!(
            "leveler-git-history-{}",
            super::super::test_ordinal()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@t"]);
        git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("src/a.rs"), "pub fn bug_marker() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "A: introduce"]);
        let first = rev(&dir, "HEAD");
        std::fs::write(dir.join("src/a.rs"), "pub fn fixed_marker() {}\n").unwrap();
        git(&dir, &["commit", "-q", "-am", "B: fix"]);
        let second = rev(&dir, "HEAD");
        // A third, uncommitted state. Historical reads must not see it.
        std::fs::write(dir.join("src/a.rs"), "pub fn dirty_worktree() {}\n").unwrap();
        Fixture { dir, first, second }
    }

    fn rev(dir: &std::path::Path, rev: &str) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(
            leveler_execution::Workspace::new(dir).unwrap(),
            leveler_execution::PermissionProfile::Assisted,
        )
    }

    async fn run(ctx: &ToolContext, input: serde_json::Value) -> ToolOutput {
        GitHistoryTool::new()
            .execute(input, ctx.clone(), CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn reads_a_file_from_the_requested_commit_not_the_worktree() {
        let f = fixture();
        let ctx = ctx(&f.dir);
        let first = run(
            &ctx,
            serde_json::json!({"action": "file", "rev": f.first, "path": "src/a.rs"}),
        )
        .await;
        assert!(first.content.contains("bug_marker"), "{}", first.content);
        assert!(!first.content.contains("dirty_worktree"));

        let second = run(
            &ctx,
            serde_json::json!({"action": "file", "rev": f.second, "path": "src/a.rs"}),
        )
        .await;
        assert!(
            second.content.contains("fixed_marker"),
            "{}",
            second.content
        );

        let worktree = std::fs::read_to_string(f.dir.join("src/a.rs")).unwrap();
        assert!(
            worktree.contains("dirty_worktree"),
            "historical reads must not touch the dirty worktree"
        );
        std::fs::remove_dir_all(&f.dir).ok();
    }

    #[tokio::test]
    async fn searches_and_lists_the_requested_tree() {
        let f = fixture();
        let ctx = ctx(&f.dir);
        let at_a = run(
            &ctx,
            serde_json::json!({"action": "search", "rev": f.first, "pattern": "bug_marker"}),
        )
        .await;
        assert!(at_a.content.contains("bug_marker"), "{}", at_a.content);

        let at_b = run(
            &ctx,
            serde_json::json!({"action": "search", "rev": f.second, "pattern": "bug_marker"}),
        )
        .await;
        assert!(
            !at_b.content.contains("bug_marker"),
            "a search at B must not see A's symbol: {}",
            at_b.content
        );

        let list = run(&ctx, serde_json::json!({"action": "list", "rev": f.first})).await;
        assert!(list.content.contains("src/a.rs"), "{}", list.content);
        std::fs::remove_dir_all(&f.dir).ok();
    }

    #[tokio::test]
    async fn log_and_blame_resolve_the_requested_commit() {
        let f = fixture();
        let ctx = ctx(&f.dir);
        let log = run(
            &ctx,
            serde_json::json!({"action": "log", "rev": f.second, "max_count": 1}),
        )
        .await;
        assert!(log.content.contains("B: fix"), "{}", log.content);

        let blame = run(
            &ctx,
            serde_json::json!({
                "action": "blame", "rev": f.first, "path": "src/a.rs",
                "line_start": 1, "line_end": 1
            }),
        )
        .await;
        assert!(blame.content.contains("bug_marker"), "{}", blame.content);
        std::fs::remove_dir_all(&f.dir).ok();
    }

    #[tokio::test]
    async fn a_merge_commit_shows_the_first_parent_diff_instead_of_an_empty_combined_diff() {
        let f = fixture();
        git(&f.dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(f.dir.join("src/b.rs"), "pub fn feature_marker() {}\n").unwrap();
        git(&f.dir, &["add", "."]);
        git(&f.dir, &["commit", "-q", "-m", "feature: add b"]);
        git(&f.dir, &["checkout", "-q", "main"]);
        std::fs::write(f.dir.join("src/c.rs"), "pub fn main_marker() {}\n").unwrap();
        git(&f.dir, &["add", "."]);
        git(&f.dir, &["commit", "-q", "-m", "main: add c"]);
        git(
            &f.dir,
            &["merge", "-q", "--no-ff", "feature", "-m", "merge feature"],
        );
        let merge = rev(&f.dir, "HEAD");

        let out = run(
            &ctx(&f.dir),
            serde_json::json!({"action": "show", "rev": merge}),
        )
        .await;
        assert!(out.content.contains("merge commit"), "{}", out.content);
        assert!(
            out.content.contains("feature_marker"),
            "the first-parent diff must carry the branch change: {}",
            out.content
        );
        std::fs::remove_dir_all(&f.dir).ok();
    }

    /// A fixed-object review must not read a later commit (for example the
    /// branch that fixed the defect). The scope is ancestry, and the default
    /// revision is the reviewed tip rather than the live HEAD.
    #[tokio::test]
    async fn a_scoped_history_tool_reads_only_the_reviewed_ancestry() {
        let f = fixture();
        let ctx = ctx(&f.dir);
        let tool = GitHistoryTool::scoped_to(f.first.clone());
        let default_rev = tool
            .execute(
                serde_json::json!({"action": "file", "path": "src/a.rs"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!default_rev.is_error, "{}", default_rev.content);
        assert!(default_rev.content.contains("bug_marker"));
        assert!(!default_rev.content.contains("fixed_marker"));

        let later = tool
            .execute(
                serde_json::json!({"action": "file", "rev": f.second, "path": "src/a.rs"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(later.is_error, "{}", later.content);
        assert!(
            later
                .content
                .contains("outside the reviewed object's ancestry"),
            "{}",
            later.content
        );

        let same = tool
            .execute(
                serde_json::json!({"action": "file", "rev": f.first, "path": "src/a.rs"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!same.is_error, "{}", same.content);
        assert!(same.content.contains("bug_marker"));
    }

    #[tokio::test]
    async fn an_unknown_revision_or_unsafe_path_is_refused_not_read_from_head() {
        let f = fixture();
        let ctx = ctx(&f.dir);
        let unknown = run(
            &ctx,
            serde_json::json!({"action": "file", "rev": "no-such-rev", "path": "src/a.rs"}),
        )
        .await;
        assert!(unknown.is_error, "{}", unknown.content);
        assert!(
            !unknown.content.contains("dirty_worktree"),
            "an unresolved revision must not fall back to the worktree: {}",
            unknown.content
        );

        let escaped = GitHistoryTool::new()
            .execute(
                serde_json::json!({"action": "file", "rev": f.first, "path": "../outside.rs"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await;
        assert!(escaped.is_err(), "traversal must be refused: {escaped:?}");
        std::fs::remove_dir_all(&f.dir).ok();
    }

    #[tokio::test]
    async fn a_file_deleted_from_the_worktree_is_still_readable_at_its_commit() {
        let f = fixture();
        let ctx = ctx(&f.dir);
        std::fs::remove_file(f.dir.join("src/a.rs")).unwrap();
        let out = run(
            &ctx,
            serde_json::json!({"action": "file", "rev": f.first, "path": "src/a.rs"}),
        )
        .await;
        assert!(out.content.contains("bug_marker"), "{}", out.content);
        std::fs::remove_dir_all(&f.dir).ok();
    }
}
