//! `replace` — literal string replacement in one file.
//!
//! It stays on the surface for one reason: `replace_all` changes every
//! occurrence in ONE call, which is a real round-trip saving on a rename that
//! `apply_patch` pays for in one hunk per occurrence. Its former rationale —
//! sparing a weaker model the context matching `apply_patch` needs — is
//! withdrawn, and whether the remaining reason is worth a third write tool is
//! the Tool Surface Closure's question (`docs/ARCHITECTURE.md` §1.1, §6.5).
//!
//! `old` matches verbatim. When it is not there, the call is refused and the
//! real text at the anchor is shown; nothing close enough is written instead.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use crate::workspace::{Commit, WorkspaceEditor};

const DESCRIPTION: &str = "Replace an exact string in a file. Use this only for a literal rename \
or exact text copied verbatim from a recent read; use apply_patch for structural edits that add, \
remove, or reshape lines. Give `path`, the exact `old` text (including whitespace/indentation), \
and the `new` replacement. By default `old` must occur exactly once; set `replace_all` to true \
to replace every occurrence in one call.";

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Path to the file, relative to the workspace root.
    path: String,
    /// The exact text to find (verbatim, including whitespace).
    old: String,
    /// The replacement text.
    new: String,
    /// Replace every occurrence. Default false: `old` must occur exactly once.
    #[serde(default)]
    replace_all: bool,
}

pub struct ReplaceTool;

#[async_trait]
impl Tool for ReplaceTool {
    fn name(&self) -> &'static str {
        "replace"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    fn mutates_files(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;

        if input.old.is_empty() {
            return Ok(ToolOutput::error("`old` must not be empty"));
        }
        if let Some(denied) = context.policy.write_path_denied(&input.path) {
            return Ok(ToolOutput::error(denied));
        }
        let resolved = match context
            .execution
            .workspace
            .resolve_for_write(&input.path, &context.write_scope())
        {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };

        let existing = match tokio::fs::read_to_string(&resolved).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(format!(
                    "cannot edit missing file: {}",
                    input.path
                )));
            }
            Err(e) => return Err(ToolError::Io(format!("read {}: {e}", input.path))),
        };

        // The `old` text was chosen against contents the model read. If the file
        // moved on since, refuse and make the model re-read (mirrors apply_patch).
        if context
            .execution
            .file_state
            .is_stale(&input.path, existing.as_bytes())
        {
            return Ok(ToolOutput::error(format!(
                "{} changed since you read it — re-read it and redo the replace against \
                 current contents.",
                input.path
            )));
        }

        let count = existing.matches(&input.old).count();
        if count == 0 {
            let head = format!(
                "`old` string not found in {}. It must match verbatim, including whitespace \
                 and indentation.",
                input.path
            );
            // The mechanical fact the model is missing is what the file really
            // says at the line it anchored on. Showing that is reporting the
            // constraint; matching something else that looked close would be
            // writing text the model never asked for.
            return Ok(ToolOutput::error(
                match crate::tools::locate_hint::real_text_at_anchor(&existing, &input.old, 3) {
                    Some(hint) => format!("{head}\n{hint}"),
                    None => format!("{head}\nNone of those lines exist in the file — re-read it."),
                },
            ));
        }
        if input.old == input.new {
            return Ok(ToolOutput::ok(format!(
                "No change: `old` and `new` are identical in {}",
                input.path
            ))
            .with_metadata(serde_json::json!({ "outcome": "no_change" })));
        }
        if count > 1 && !input.replace_all {
            return Ok(ToolOutput::error(format!(
                "`old` occurs {count} times in {} — ambiguous. Add surrounding context to make \
                 it unique, or set replace_all=true to change every occurrence.",
                input.path
            )));
        }
        let new_content = if input.replace_all {
            existing.replace(&input.old, &input.new)
        } else {
            existing.replacen(&input.old, &input.new, 1)
        };

        match WorkspaceEditor::replace(&context, &resolved, &existing, &new_content).await? {
            Commit::Written => {}
            Commit::Stale => {
                return Ok(ToolOutput::error(format!(
                    "{} changed since you read it — re-read it and redo the replace against current contents.",
                    input.path
                )));
            }
            Commit::Rejected(message) => return Ok(ToolOutput::error(message)),
        }

        // Re-fingerprint so our own edit isn't seen as an outside change next time.
        context
            .execution
            .file_state
            .record(&input.path, new_content.as_bytes());
        // Auto-format the edited file (best-effort; re-fingerprints internally).

        // Where the replacement landed. `replace` rewrites a substring the
        // model matched by content, so the line numbers exist only here — the
        // arguments carry none, which is why its inline diff used to render
        // without a gutter.
        //
        // Report the write too: the executor folds `modified_files` into the
        // turn's change set, and the engine gates verification on it. Without
        // this an edit made here is invisible and the run finishes unverified.
        let hunks = super::applied_diff::literal_replacement_hunks(
            &existing,
            &input.old,
            &input.new,
            input.replace_all,
        );
        let mut meta = serde_json::json!({ "modified_files": [input.path.clone()] });
        if let Some(diff) = super::applied_diff::unified_diff(&input.path, &hunks) {
            meta["applied_diff"] = serde_json::Value::String(diff);
        }
        Ok(ToolOutput::ok(format!(
            "Replaced {count} occurrence{} in {}",
            if count == 1 { "" } else { "s" },
            input.path
        ))
        .with_metadata(meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(contents: &str) -> (ToolContext, std::path::PathBuf) {
        super::super::test_ctx(
            leveler_execution::PermissionProfile::Assisted,
            &[("src/lib.rs", contents)],
        )
    }

    async fn run(ctx: ToolContext, args: serde_json::Value) -> ToolOutput {
        ReplaceTool
            .execute(args, ctx, CancellationToken::new())
            .await
            .unwrap()
    }

    /// The executor learns a file changed ONLY from a tool's `modified_files`
    /// metadata; the engine gates verification on that list. `apply_patch`
    /// reports it, `replace` did not — so a model that edited via `replace`
    /// sailed past the verification gate and the run was declared
    /// CompletedUnverified with real, unverified edits on disk. (Caught by the
    /// L1 P0 smoke run: ts-t1-01.)
    /// The UI renders an edit from WHERE it landed. `replace` matches a
    /// substring, so nothing in the request says which line that was — the
    /// tool has to report it, or the inline diff has no line numbers at all.
    #[tokio::test]
    async fn a_successful_replace_reports_where_the_change_landed() {
        let (c, dir) = ctx("one\ntwo\nthree\nfour\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "three", "new": "THREE"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        let diff = out.metadata["applied_diff"].as_str().expect("applied diff");
        assert!(diff.contains("@@ -3,1 +3,1 @@"), "{diff}");
        assert!(diff.contains("-three") && diff.contains("+THREE"), "{diff}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_successful_replace_reports_the_file_it_modified() {
        let (c, dir) = ctx("alpha beta gamma\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "beta", "new": "BETA"}),
        )
        .await;

        assert!(!out.is_error, "should succeed: {}", out.content);
        let modified = out
            .metadata
            .get("modified_files")
            .and_then(|v| v.as_array())
            .expect("a write tool must report modified_files, or verification is skipped");
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].as_str(), Some("src/lib.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A refused replace changed nothing — it must not claim it did.
    #[tokio::test]
    async fn a_refused_replace_reports_no_modified_file() {
        let (c, dir) = ctx("hello world\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "zzz", "new": "q"}),
        )
        .await;

        assert!(out.is_error);
        assert!(
            out.metadata.get("modified_files").is_none(),
            "a failed edit must not report a modified file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn identical_old_and_new_is_a_successful_noop() {
        let (c, dir) = ctx("hello world\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "hello", "new": "hello"}),
        )
        .await;

        assert!(!out.is_error, "a valid no-op is not an execution failure");
        assert!(out.content.to_lowercase().contains("no change"));
        assert!(out.metadata.get("modified_files").is_none());
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "hello world\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn description_reserves_replace_for_literal_renames() {
        assert!(DESCRIPTION.contains("literal rename"), "{DESCRIPTION}");
        assert!(DESCRIPTION.contains("apply_patch"), "{DESCRIPTION}");
        assert!(
            !DESCRIPTION.contains("Prefer this over apply_patch"),
            "{DESCRIPTION}"
        );
    }

    #[tokio::test]
    async fn replaces_a_unique_occurrence() {
        let (c, dir) = ctx("alpha beta gamma\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "beta", "new": "BETA"}),
        )
        .await;
        assert!(!out.is_error, "should succeed: {}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "alpha BETA gamma\n"
        );
    }

    #[tokio::test]
    async fn replace_all_renames_every_occurrence() {
        let (c, dir) = ctx("old() old() old()\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "old", "new": "renamed", "replace_all": true}),
        )
        .await;
        assert!(!out.is_error, "should succeed: {}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "renamed() renamed() renamed()\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_without_replace_all_is_refused() {
        let (c, dir) = ctx("x x\n");
        let out = run(
            c.clone(),
            serde_json::json!({"path": "src/lib.rs", "old": "x", "new": "y"}),
        )
        .await;
        assert!(out.is_error, "ambiguous replace must be refused");
        assert!(
            out.content.contains("2") || out.content.to_lowercase().contains("occur"),
            "error names the ambiguity: {}",
            out.content
        );
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "x x\n"
        );
    }

    #[tokio::test]
    async fn missing_old_string_is_refused() {
        let (c, dir) = ctx("hello world\n");
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "zzz", "new": "q"}),
        )
        .await;
        assert!(out.is_error, "not-found must be refused");
        assert!(
            out.content.to_lowercase().contains("not found") || out.content.contains("zzz"),
            "error explains: {}",
            out.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "hello world\n"
        );
    }

    #[tokio::test]
    async fn refuses_a_path_outside_the_workspace() {
        let (c, _dir) = ctx("data\n");
        let out = run(
            c,
            serde_json::json!({"path": "../escape.txt", "old": "data", "new": "x"}),
        )
        .await;
        assert!(out.is_error, "must reject a path outside the workspace");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_replaces_cannot_both_commit_from_the_same_version() {
        let (first, dir) = ctx("value = old\n");
        let second =
            super::super::test_ctx_in(&dir, leveler_execution::PermissionProfile::Assisted);
        let gate = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let launch = |context: ToolContext,
                      replacement: &'static str,
                      gate: std::sync::Arc<tokio::sync::Barrier>| async move {
            gate.wait().await;
            run(
                context,
                serde_json::json!({
                    "path": "src/lib.rs", "old": "old", "new": replacement
                }),
            )
            .await
        };
        let a = tokio::spawn(launch(first, "first", gate.clone()));
        let b = tokio::spawn(launch(second, "second", gate.clone()));
        gate.wait().await;
        let (a, b) = (a.await.unwrap(), b.await.unwrap());

        assert_ne!(
            a.is_error, b.is_error,
            "exactly one stale writer must be rejected"
        );
        let final_text = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(matches!(
            final_text.as_str(),
            "value = first\n" | "value = second\n"
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The write lock must never touch the workspace: no `.leveler-lock`
    /// file may appear next to the target, and (on unix) the global lock
    /// file under `<home>/locks/` is unlinked once the replace completes.
    #[tokio::test]
    async fn replace_leaves_no_lock_files_behind() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-replace-lockfree-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "value = old\n").unwrap();
        let home = dir.join("leveler-home");
        let env = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os().chain([(
                std::ffi::OsString::from("LEVELER_HOME"),
                home.clone().into_os_string(),
            )]),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ));
        let c = ToolContext::with_environment(
            leveler_execution::Workspace::new(&dir).unwrap(),
            leveler_execution::PermissionProfile::Assisted,
            env,
        );
        let out = run(
            c,
            serde_json::json!({"path": "src/lib.rs", "old": "old", "new": "new"}),
        )
        .await;
        assert!(!out.is_error, "replace failed: {}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "value = new\n"
        );

        let residue: Vec<String> = std::fs::read_dir(dir.join("src"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("leveler-lock"))
            .collect();
        assert!(residue.is_empty(), "lock residue in workspace: {residue:?}");

        #[cfg(unix)]
        {
            let leftover: Vec<std::path::PathBuf> = std::fs::read_dir(home.join("locks"))
                .map(|it| it.filter_map(Result::ok).map(|e| e.path()).collect())
                .unwrap_or_default();
            assert!(
                leftover.is_empty(),
                "lock files must be unlinked on release: {leftover:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The near miss the fuzzy fallback used to absorb. Typographic quotes on
    /// disk, ASCII quotes in `old`: the call is refused and the file is left
    /// alone, because writing a string the model did not ask for is worse than
    /// costing it one more round.
    #[tokio::test]
    async fn a_near_miss_is_refused_rather_than_guessed() {
        let (c, dir) = ctx("let title = \u{201C}Deploy\u{201D};\n");
        let out = run(
            c,
            serde_json::json!({
                "path": "src/lib.rs",
                "old": "let title = \"Deploy\";",
                "new": "let title = \"Ship\";",
            }),
        )
        .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains("verbatim"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "let title = \u{201C}Deploy\u{201D};\n",
            "nothing may be written when the literal text was not found"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_missing_old_string_shows_what_the_file_really_contains() {
        let (c, dir) = ctx("fn main() {\n    let total = a + b;\n    println!(\"{total}\");\n}\n");
        // The model approximates the line it read: right anchor, wrong body.
        let out = run(
            c,
            serde_json::json!({
                "path": "src/lib.rs",
                "old": "    let total = a+b;",
                "new": "    let total = a.checked_add(b)?;",
            }),
        )
        .await;

        assert!(out.is_error);
        assert!(
            out.content.contains("2\u{2502}     let total = a + b;"),
            "the error must show the real line, with its number, so the model can copy it \
             back verbatim instead of guessing again; got:\n{}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
