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

Every hunk is planned before the first write. Commits use compare-and-swap; if a
later commit conflicts, earlier commits are rolled back without overwriting the
conflicting writer (and a rollback conflict is surfaced loudly). So after a
call succeeds, do NOT re-read the file to check the edit landed — a hunk that
failed to match fails the whole call loudly. Re-reading only burns context."#;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The full patch document, starting with `*** Begin Patch`.
    patch: String,
}

/// A resolved filesystem operation, computed before anything is written.
enum Op {
    /// Update an existing file in place: commit compares the on-disk bytes
    /// against `expected` (what the plan phase read) before swapping in
    /// `content`, so a concurrent writer can't be silently clobbered.
    Replace {
        path: PathBuf,
        expected: String,
        content: String,
    },
    /// Create a new file (`Add File`, or an `Update … Move to:` destination):
    /// no prior version to match against.
    Create { path: PathBuf, content: String },
    Remove {
        path: PathBuf,
        expected: String,
        permissions: std::fs::Permissions,
    },
}

enum Applied {
    Replaced {
        path: PathBuf,
        before: String,
        after: String,
    },
    Created {
        path: PathBuf,
        content: String,
    },
    Removed {
        path: PathBuf,
        content: String,
        permissions: std::fs::Permissions,
    },
}

enum CommitFailure {
    Model(String),
    Infrastructure(ToolError),
}

async fn commit_op(context: &ToolContext, op: Op) -> Result<Applied, CommitFailure> {
    match op {
        Op::Replace {
            path,
            expected,
            content,
        } => match super::replace::commit_replace(context, &path, &expected, &content).await {
            Ok(super::replace::Commit::Written) => Ok(Applied::Replaced {
                path,
                before: expected,
                after: content,
            }),
            Ok(super::replace::Commit::Stale) => Err(CommitFailure::Model(format!(
                "{} changed on disk between planning and writing this patch — another process or \
                 command edited it. Re-read it and rebuild the patch against what is there now.",
                path.display()
            ))),
            Ok(super::replace::Commit::Rejected(message)) => Err(CommitFailure::Model(message)),
            Err(error) => Err(CommitFailure::Infrastructure(error)),
        },
        Op::Create { path, content } => {
            match super::replace::commit_create(context, &path, &content).await {
                Ok(super::replace::Commit::Written) => Ok(Applied::Created { path, content }),
                Ok(super::replace::Commit::Stale) => Err(CommitFailure::Model(format!(
                    "{} was created by another writer while this patch was being committed",
                    path.display()
                ))),
                Ok(super::replace::Commit::Rejected(message)) => Err(CommitFailure::Model(message)),
                Err(error) => Err(CommitFailure::Infrastructure(error)),
            }
        }
        Op::Remove {
            path,
            expected,
            permissions,
        } => match super::replace::commit_remove(context, &path, &expected).await {
            Ok(super::replace::Commit::Written) => Ok(Applied::Removed {
                path,
                content: expected,
                permissions,
            }),
            Ok(super::replace::Commit::Stale) => Err(CommitFailure::Model(format!(
                "{} changed on disk between planning and deleting it — another process or \
                     command edited it. Re-read it and rebuild the patch against what is there now.",
                path.display()
            ))),
            Ok(super::replace::Commit::Rejected(message)) => Err(CommitFailure::Model(message)),
            Err(error) => Err(CommitFailure::Infrastructure(error)),
        },
    }
}

async fn rollback_applied(context: &ToolContext, applied: &[Applied]) -> Result<(), ToolError> {
    for operation in applied.iter().rev() {
        let (path, outcome) = match operation {
            Applied::Replaced {
                path,
                before,
                after,
            } => (
                path,
                super::replace::commit_replace(context, path, after, before).await?,
            ),
            Applied::Created { path, content } => (
                path,
                super::replace::commit_remove(context, path, content).await?,
            ),
            Applied::Removed {
                path,
                content,
                permissions,
            } => (
                path,
                super::replace::commit_create_with_permissions(
                    context,
                    path,
                    content,
                    Some(permissions.clone()),
                )
                .await?,
            ),
        };
        match outcome {
            super::replace::Commit::Written => {}
            super::replace::Commit::Stale => {
                return Err(ToolError::Io(format!(
                    "rollback refused to overwrite a concurrent change at {}",
                    path.display()
                )));
            }
            super::replace::Commit::Rejected(message) => {
                return Err(ToolError::Io(format!(
                    "rollback rejected {}: {message}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
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
        cancellation: CancellationToken,
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
                    match tokio::fs::symlink_metadata(&resolved).await {
                        Ok(_) => {
                            return Ok(ToolOutput::error(format!(
                                "cannot add existing file: {path}"
                            )));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(ToolError::Io(format!("stat {path}: {e}"))),
                    }
                    ops.push(Op::Create {
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
                    let expected = match tokio::fs::read_to_string(&resolved).await {
                        Ok(content) => content,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            return Ok(ToolOutput::error(format!(
                                "cannot delete missing file: {path}"
                            )));
                        }
                        Err(e) => return Err(ToolError::Io(format!("read {path}: {e}"))),
                    };
                    let permissions = tokio::fs::symlink_metadata(&resolved)
                        .await
                        .map_err(|e| ToolError::Io(format!("stat {path}: {e}")))?
                        .permissions();
                    ops.push(Op::Remove {
                        path: resolved,
                        expected,
                        permissions,
                    });
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
                    let existing_permissions = tokio::fs::symlink_metadata(&resolved)
                        .await
                        .map_err(|e| ToolError::Io(format!("stat {path}: {e}")))?
                        .permissions();

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
                            match tokio::fs::symlink_metadata(&dest_resolved).await {
                                Ok(_) => {
                                    return Ok(ToolOutput::error(format!(
                                        "cannot move to existing file: {dest}"
                                    )));
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                Err(e) => {
                                    return Err(ToolError::Io(format!("stat {dest}: {e}")));
                                }
                            }
                            // Create the destination first. If the source CAS
                            // later fails, rollback can safely remove only the
                            // exact destination bytes this patch created.
                            ops.push(Op::Create {
                                path: dest_resolved,
                                content: updated,
                            });
                            ops.push(Op::Remove {
                                path: resolved,
                                expected: existing,
                                permissions: existing_permissions,
                            });
                            summary.push(format!("M {path} -> {dest}"));
                            modified.push(path);
                            modified.push(dest);
                        }
                        None => {
                            ops.push(Op::Replace {
                                path: resolved,
                                expected: existing,
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

        // Commit phase — now that everything resolved, write to disk through the
        // shared lock + compare-and-swap engine (same as `replace`): an in-place
        // Update refuses if the file moved on disk since the plan phase read it,
        // so a concurrent writer is never silently clobbered. Each file is
        // checkpointed just before it is first modified (spec §28).
        let mut applied = Vec::new();
        for op in ops {
            match commit_op(&context, op).await {
                Ok(operation) => applied.push(operation),
                Err(failure) => {
                    if let Err(rollback_error) = rollback_applied(&context, &applied).await {
                        let original = match failure {
                            CommitFailure::Model(message) => message,
                            CommitFailure::Infrastructure(error) => error.to_string(),
                        };
                        return Err(ToolError::Io(format!(
                            "patch commit failed ({original}); rollback also failed: \
                             {rollback_error}"
                        )));
                    }
                    return match failure {
                        CommitFailure::Model(message) => Ok(ToolOutput::error(message)),
                        CommitFailure::Infrastructure(error) => Err(error),
                    };
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
                        super::format::format_after_edit(&context, rel, &resolved, &cancellation)
                            .await;
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

    /// Two independent contexts patch the same file, from the same read
    /// version, at the same time. Without a cross-process lock + compare-and-swap
    /// at commit, both writes "succeed" and the second silently clobbers the
    /// first. Mirrors `replace`'s concurrency guarantee: exactly one must be
    /// rejected as stale.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_patches_cannot_both_commit_from_the_same_version() {
        let (first, dir) = ctx();
        let second = ToolContext::new(
            leveler_execution::Workspace::new(&dir).unwrap(),
            leveler_execution::PermissionProfile::Assisted,
        );
        // Both contexts read the file, so neither trips the in-process staleness
        // guard — the only thing that can reject a writer is the commit-time CAS.
        for c in [&first, &second] {
            crate::tools::read_file::ReadFileTool
                .execute(
                    serde_json::json!({ "path": "src/lib.rs" }),
                    c.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }

        let gate = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let launch = |context: ToolContext,
                      body: &'static str,
                      gate: std::sync::Arc<tokio::sync::Barrier>| async move {
            let patch = format!(
                "*** Begin Patch\n*** Update File: src/lib.rs\n fn a() {{}}\n-fn b() {{}}\n+fn b() {{ {body} }}\n*** End Patch"
            );
            gate.wait().await;
            ApplyPatchTool
                .execute(
                    serde_json::json!({ "patch": patch }),
                    context,
                    CancellationToken::new(),
                )
                .await
                .unwrap()
        };
        let a = tokio::spawn(launch(first, "A()", gate.clone()));
        let b = tokio::spawn(launch(second, "B()", gate.clone()));
        gate.wait().await;
        let (a, b) = (a.await.unwrap(), b.await.unwrap());

        assert_ne!(
            a.is_error, b.is_error,
            "exactly one stale writer must be rejected; got a.err={} b.err={}\n{}\n{}",
            a.is_error, b.is_error, a.content, b.content
        );
        let final_text = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(
            matches!(
                final_text.as_str(),
                "fn a() {}\nfn b() { A() }\n" | "fn a() {}\nfn b() { B() }\n"
            ),
            "one writer's content must survive intact, not a torn mix: {final_text:?}"
        );
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
    async fn add_refuses_to_overwrite_an_existing_file() {
        let (context, dir) = ctx();
        let existing = dir.join("src/existing.rs");
        std::fs::write(&existing, "keep me\n").unwrap();
        let patch = "*** Begin Patch\n*** Add File: src/existing.rs\n+replacement\n*** End Patch";

        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(out.is_error, "Add File must reject an existing target");
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "keep me\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn move_reports_both_source_and_destination_as_modified() {
        let (context, dir) = ctx();
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n*** Move to: src/moved.rs\n fn a() {}\n fn b() {}\n*** End Patch";

        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(!out.is_error, "{}", out.content);
        let modified = out
            .metadata
            .get("modified_files")
            .and_then(serde_json::Value::as_array)
            .expect("modified_files metadata");
        assert!(
            modified.iter().any(|path| path == "src/lib.rs"),
            "move source must be reported: {modified:?}"
        );
        assert!(
            modified.iter().any(|path| path == "src/moved.rs"),
            "move destination must be reported: {modified:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn move_refuses_an_existing_destination_without_deleting_the_source() {
        let (context, dir) = ctx();
        let destination = dir.join("src/moved.rs");
        std::fs::write(&destination, "destination\n").unwrap();
        let patch = "*** Begin Patch\n*** Update File: src/lib.rs\n*** Move to: src/moved.rs\n fn a() {}\n fn b() {}\n*** End Patch";

        let out = ApplyPatchTool
            .execute(
                serde_json::json!({ "patch": patch }),
                context,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(out.is_error, "move must reject an existing destination");
        assert!(
            dir.join("src/lib.rs").exists(),
            "a rejected move must keep its source"
        );
        assert_eq!(
            std::fs::read_to_string(destination).unwrap(),
            "destination\n"
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_delete_rolls_back_earlier_commits_without_clobbering_external_changes() {
        use fs2::FileExt;

        let (context, dir) = ctx();
        let second = dir.join("src/second.rs");
        std::fs::write(&second, "second-original\n").unwrap();

        // Hold the second target's cooperative lock so the patch commits its
        // first delete and then pauses before comparing/removing the second.
        let lock_path = leveler_project::layout::target_lock_path(&context.environment, &second);
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        lock.lock_exclusive().unwrap();

        let patch = "*** Begin Patch\n*** Delete File: src/lib.rs\n*** Delete File: src/second.rs\n*** End Patch";
        let task = tokio::spawn({
            let context = context.clone();
            async move {
                ApplyPatchTool
                    .execute(
                        serde_json::json!({ "patch": patch }),
                        context,
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap()
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while dir.join("src/lib.rs").exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first operation should commit before the second lock");

        std::fs::write(&second, "external-change\n").unwrap();
        FileExt::unlock(&lock).unwrap();
        drop(lock);

        let out = task.await.unwrap();
        assert!(
            out.is_error,
            "the stale second delete must reject the patch"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
            "fn a() {}\nfn b() {}\n",
            "an earlier committed operation must be rolled back"
        );
        assert_eq!(
            std::fs::read_to_string(&second).unwrap(),
            "external-change\n",
            "rollback must not overwrite the concurrent writer"
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
