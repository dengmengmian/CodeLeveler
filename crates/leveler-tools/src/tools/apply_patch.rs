//! `apply_patch` — apply a restricted `*** Begin Patch` document (spec §18.3).
//!
//! Planning is all-or-nothing: every change is resolved and computed in memory
//! first, so a hunk that fails to match aborts the whole patch without touching
//! the filesystem. Checkpoints are handled before the first write.

use std::path::PathBuf;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use super::patch::{FileChange, apply_update, parse_patch};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// Precise format spec + example given to the model. Weaker models must be told
/// the exact grammar; unified diff is accepted as a compatibility adapter.
const DESCRIPTION: &str = r#"Edit workspace files. Prefer this exact format:

*** Begin Patch
<one or more file sections>
*** End Patch

File section headers (the leading '*** ' is REQUIRED):
  *** Add File: <path>       -> then every new line prefixed with '+'
  *** Delete File: <path>    -> no body
  *** Update File: <path>    -> then hunks; may be followed by '*** Move to: <newpath>'

In an Update File hunk, prefix EVERY line:
  ' ' (space) = unchanged context line to keep
  '-'         = line to remove
  '+'         = line to add
Include 2-3 unchanged context lines around the edit so it can be located. The
context and '-' lines must match the file EXACTLY (copy them; do not retype).
When those lines are not unique in the file, add an anchor line '@@ <label>'
before the hunk naming an enclosing line (e.g. '@@ fn handle_request') so the
hunk is located inside that scope:
  @@ fn handle_request
       let body = ...
  -    parse(body)
  +    parse(body)?
To append new lines at the end of a file, use a hunk with only '+' lines and no
context (no '@@', no ' '/'-' lines).
Unified diff is also accepted for simple add/update/delete patches (`--- a/file`,
`+++ b/file`, `@@ -1,3 +1,3 @@`). Do NOT use search/replace or merge-conflict
markers ('=======', '<<<<<<<', '>>>>>>>'). Do NOT wrap the patch in markdown
code fences (```). Paths are relative to the workspace root.

Example (add a function after an existing one):

*** Begin Patch
*** Update File: src/lib.rs
 pub fn add(a: i32, b: i32) -> i32 {
     a + b
 }
+
+pub fn subtract(a: i32, b: i32) -> i32 {
+    a - b
+}
*** End Patch

The patch applies atomically: if any hunk cannot be located, nothing is written.
So after a call succeeds, do NOT re-read the file to check the edit landed — a
hunk that failed to match fails the whole call loudly. Re-reading only burns
context."#;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The full patch document, starting with `*** Begin Patch`.
    patch: String,
}

/// A resolved filesystem operation, computed before anything is written.
enum Op {
    Write { path: PathBuf, content: String },
    Remove { path: PathBuf },
}

pub struct ApplyPatchTool;

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &'static str {
        "apply_patch"
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

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;

        let changes = match parse_patch(&input.patch) {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };

        // Plan phase — resolve and compute all operations in memory.
        let mut ops = Vec::new();
        let mut summary = Vec::new();
        let mut modified = Vec::new();

        for change in changes {
            match change {
                FileChange::Add { path, content } => {
                    let resolved = match context.workspace.resolve(&path) {
                        Ok(p) => p,
                        Err(e) => return Ok(ToolOutput::error(e.to_string())),
                    };
                    ops.push(Op::Write {
                        path: resolved,
                        content,
                    });
                    summary.push(format!("A {path}"));
                    modified.push(path);
                }
                FileChange::Delete { path } => {
                    let resolved = match context.workspace.resolve(&path) {
                        Ok(p) => p,
                        Err(e) => return Ok(ToolOutput::error(e.to_string())),
                    };
                    if !resolved.exists() {
                        return Ok(ToolOutput::error(format!(
                            "cannot delete missing file: {path}"
                        )));
                    }
                    ops.push(Op::Remove { path: resolved });
                    summary.push(format!("D {path}"));
                    modified.push(path);
                }
                FileChange::Update {
                    path,
                    move_to,
                    chunks,
                } => {
                    let resolved = match context.workspace.resolve(&path) {
                        Ok(p) => p,
                        Err(e) => return Ok(ToolOutput::error(e.to_string())),
                    };
                    let existing = match tokio::fs::read_to_string(&resolved).await {
                        Ok(s) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            return Ok(ToolOutput::error(format!(
                                "cannot update missing file: {path}"
                            )));
                        }
                        Err(e) => return Err(ToolError::Io(format!("read {path}: {e}"))),
                    };

                    // The patch was written against contents the model read. If
                    // the file moved on since, applying it would discard whatever
                    // the other writer did — refuse and make the model re-read.
                    if context.file_state.is_stale(&path, existing.as_bytes()) {
                        return Ok(ToolOutput::error(format!(
                            "{path} changed since you read it — another process, command, or \
                             the user edited it. Your patch was written against stale contents \
                             and applying it would discard their change. Re-read {path} and \
                             rebuild the patch against what is there now."
                        )));
                    }

                    let updated = match apply_update(&existing, &chunks) {
                        Ok(s) => s,
                        Err(reason) => {
                            return Ok(ToolOutput::error(format!(
                                "failed to apply hunk to {path}: {reason}"
                            )));
                        }
                    };

                    match move_to {
                        Some(dest) => {
                            let dest_resolved = match context.workspace.resolve(&dest) {
                                Ok(p) => p,
                                Err(e) => return Ok(ToolOutput::error(e.to_string())),
                            };
                            ops.push(Op::Remove { path: resolved });
                            ops.push(Op::Write {
                                path: dest_resolved,
                                content: updated,
                            });
                            summary.push(format!("M {path} -> {dest}"));
                            modified.push(dest);
                        }
                        None => {
                            ops.push(Op::Write {
                                path: resolved,
                                content: updated,
                            });
                            summary.push(format!("M {path}"));
                            modified.push(path);
                        }
                    }
                }
            }
        }

        // Enforce the model-policy per-step file cap (spec §17): weaker models
        // are kept to small, reviewable edits.
        if context.max_files_per_step > 0 {
            let distinct: std::collections::BTreeSet<&String> = modified.iter().collect();
            if distinct.len() > context.max_files_per_step {
                return Ok(ToolOutput::error(format!(
                    "this patch changes {} files but the per-step limit is {}; \
                     make a smaller patch touching fewer files",
                    distinct.len(),
                    context.max_files_per_step
                )));
            }
        }

        // Task-level residual file budget (epoch): a single multi-file patch
        // must not introduce more *new* paths than remain, even when under the
        // per-step model-policy cap.
        if let Some(remaining) = context.command_modified_files_remaining {
            let previously: std::collections::BTreeSet<&str> = context
                .command_previously_modified
                .iter()
                .map(String::as_str)
                .collect();
            let newly: std::collections::BTreeSet<&str> = modified
                .iter()
                .map(String::as_str)
                .filter(|p| !previously.contains(*p))
                .collect();
            if newly.len() > remaining {
                return Ok(ToolOutput::error(format!(
                    "this patch would modify {} new file(s) but only {remaining} remain in the \
                     task file budget; make a smaller patch",
                    newly.len()
                )));
            }
        }

        // Commit phase — now that everything resolved, write to disk. Each file
        // is checkpointed just before it is first modified (spec §28).
        for op in ops {
            match op {
                Op::Write { path, content } => {
                    context.checkpoint.record(&path);
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|e| {
                            ToolError::Io(format!("mkdir {}: {e}", parent.display()))
                        })?;
                    }
                    // Atomic write: stage into a sibling temp file, then rename
                    // over the target so a crash mid-write never leaves a
                    // half-written (corrupt) source file (atomcode semantics).
                    let tmp = path.with_extension(format!(
                        "{}.leveler-tmp",
                        path.extension().and_then(|e| e.to_str()).unwrap_or("")
                    ));
                    tokio::fs::write(&tmp, content)
                        .await
                        .map_err(|e| ToolError::Io(format!("write {}: {e}", tmp.display())))?;
                    tokio::fs::rename(&tmp, &path).await.map_err(|e| {
                        ToolError::Io(format!("rename into {}: {e}", path.display()))
                    })?;
                }
                Op::Remove { path } => {
                    context.checkpoint.record(&path);
                    tokio::fs::remove_file(&path)
                        .await
                        .map_err(|e| ToolError::Io(format!("remove {}: {e}", path.display())))?;
                }
            }
        }

        // Re-fingerprint what we just wrote, so the agent's own edit does not look
        // like an outside change to its next patch. A path we can no longer read
        // was deleted here; forget it so a recreated file starts clean.
        for rel in &modified {
            match context.workspace.resolve(rel) {
                Ok(resolved) => match tokio::fs::read(&resolved).await {
                    Ok(bytes) => {
                        context.file_state.record(rel, &bytes);
                        // Auto-format the edited file (best-effort; re-fingerprints).
                        super::format::format_after_edit(&context, rel, &resolved).await;
                    }
                    Err(_) => context.file_state.forget(rel),
                },
                Err(_) => context.file_state.forget(rel),
            }
        }

        let body = format!("Applied patch:\n{}\n", summary.join("\n"));
        Ok(ToolOutput::ok(body).with_metadata(serde_json::json!({ "modified_files": modified })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The patch is atomic, so a failed hunk fails the call. A model that
    /// re-reads the file "to check" after every successful patch burns context
    /// for nothing — the tool description has to say so, or it will.
    #[test]
    fn description_forbids_re_reading_the_file_to_verify_the_edit() {
        assert!(DESCRIPTION.contains("do NOT re-read the file"));
        assert!(DESCRIPTION.contains("fails the whole call"));
    }

    fn ctx() -> (ToolContext, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("leveler-patch-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        (
            ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted),
            dir,
        )
    }

    /// Read a file, let something else rewrite it, then patch it. The patch must
    /// be refused: the model's context describes a file that no longer exists on
    /// disk, so applying it would silently discard the other writer's change.
    #[tokio::test]
    async fn rejects_a_patch_to_a_file_changed_since_it_was_read() {
        let (context, dir) = ctx();

        crate::tools::read_file::ReadFileTool
            .execute(
                serde_json::json!({ "path": "src/lib.rs" }),
                context.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        // Someone else (a run_command, or the user) rewrites the file.
        std::fs::write(dir.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();

        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n fn a() {}\n-fn b() {}\n+fn b() { todo!() }\n*** End Patch";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(out.is_error, "stale patch must be refused: {}", out.content);
        assert!(
            out.content.contains("changed since"),
            "error must explain why: {}",
            out.content
        );
        // The other writer's content survives untouched.
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "fn a() {}\nfn b() {}\nfn c() {}\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The guard must not fire on an unmodified file, or every read→edit is broken.
    #[tokio::test]
    async fn allows_a_patch_to_a_file_untouched_since_it_was_read() {
        let (context, dir) = ctx();
        crate::tools::read_file::ReadFileTool
            .execute(
                serde_json::json!({ "path": "src/lib.rs" }),
                context.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n fn a() {}\n-fn b() {}\n+fn b() { todo!() }\n*** End Patch";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two edits in a row: the first write must refresh the tracked fingerprint,
    /// otherwise apply_patch would flag its own edit as an outside change.
    #[tokio::test]
    async fn consecutive_patches_do_not_trip_the_guard() {
        let (context, dir) = ctx();
        crate::tools::read_file::ReadFileTool
            .execute(
                serde_json::json!({ "path": "src/lib.rs" }),
                context.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        for patch in [
            "*** Begin Patch\n*** Update File: src/lib.rs\n fn a() {}\n-fn b() {}\n+fn b() { todo!() }\n*** End Patch",
            "*** Begin Patch\n*** Update File: src/lib.rs\n-fn a() {}\n+fn a() { todo!() }\n fn b() { todo!() }\n*** End Patch",
        ] {
            let out = ApplyPatchTool
                .execute(
                    serde_json::json!({ "patch": patch }),
                    context.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(!out.is_error, "{}", out.content);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn adds_a_file() {
        let (context, dir) = ctx();
        let patch = "*** Begin Patch\n*** Add File: src/new.rs\n+pub fn c() {}\n*** End Patch";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/new.rs")).unwrap(),
            "pub fn c() {}\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn updates_a_file() {
        let (context, dir) = ctx();
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n fn a() {}\n-fn b() {}\n+fn b() { todo!() }\n*** End Patch";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "fn a() {}\nfn b() { todo!() }\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn accepts_unified_diff_update() {
        let (context, dir) = ctx();
        let patch = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
 fn a() {}
-fn b() {}
+fn b() { todo!() }
";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "fn a() {}\nfn b() { todo!() }\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn failed_hunk_writes_nothing() {
        let (context, dir) = ctx();
        let before = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        let patch =
            "*** Begin Patch\n*** Update File: src/lib.rs\n-nonexistent line\n+x\n*** End Patch";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            before
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn enforces_max_files_per_step() {
        let (context, dir) = ctx();
        let context = context.with_policy_limits(2, true); // cap at 2 files
        let patch = "*** Begin Patch\n*** Add File: a.rs\n+a\n*** Add File: b.rs\n+b\n*** Add File: c.rs\n+c\n*** End Patch";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("per-step limit"));
        // Nothing should have been written (atomic).
        assert!(!dir.join("a.rs").exists());
        assert!(!dir.join("c.rs").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn enforces_task_file_budget_on_new_paths() {
        let (context, dir) = ctx();
        // Residual 1 new file; patch tries two new files → refuse before write.
        let context = context.with_command_write_constraints(None, Some(1), vec![]);
        let patch =
            "*** Begin Patch\n*** Add File: a.rs\n+a\n*** Add File: b.rs\n+b\n*** End Patch";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("task file budget") || out.content.contains("new file"),
            "{}",
            out.content
        );
        assert!(!dir.join("a.rs").exists());
        assert!(!dir.join("b.rs").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn end_to_end_tolerates_indented_header_and_appends_at_eof() {
        // Exercises the real filesystem write path with several
        // tolerances at once: an indented `*** Update File:` header and a
        // context-less pure addition (append at EOF) on a file that has NO
        // trailing newline. The result must be written correctly and normalized.
        let dir = std::env::temp_dir().join(format!(
            "leveler-patch-e2e-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "alpha").unwrap(); // no trailing newline
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let context = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);

        let patch =
            "*** Begin Patch\n  *** Update File: notes.txt\n@@\n+beta\n+gamma\n*** End Patch";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn rejects_out_of_workspace() {
        let (context, dir) = ctx();
        let patch = "*** Begin Patch\n*** Add File: ../escape.rs\n+x\n*** End Patch";
        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        std::fs::remove_dir_all(&dir).ok();
    }
}
