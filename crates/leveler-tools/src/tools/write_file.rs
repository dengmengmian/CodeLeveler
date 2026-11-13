//! `write_file` — the `write` primitive: materialize a complete file.
//!
//! Creating a file, and deliberately replacing one end to end, is a different
//! model intent from changing part of one. That is why this is a primitive
//! beside `apply_patch` rather than a convenience over it — and it is the only
//! reason. It goes through the same [`WorkspaceEditor`] as every other
//! mutation, so the write scope, the compare-and-swap, the checkpoint and
//! rollback are identical guarantees.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use crate::workspace::{Commit, WorkspaceEditor};

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Path to the file, relative to the workspace root.
    path: String,
    /// The file's complete new contents.
    content: String,
    /// Set only when the user explicitly asked for the literal text
    /// [REDACTED]. Defaults to false: that token is normally the runtime's
    /// placeholder for a hidden secret and must not be written to disk.
    #[serde(default)]
    allow_redacted_literal: bool,
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Write a file's complete contents, creating it if it does not exist. \
         `content` becomes the whole file — anything already there is replaced. \
         Use this to create a new file or to deliberately rewrite one end to \
         end; use `apply_patch` to change part of an existing file. Overwriting \
         a file that changed since you read it is refused. [REDACTED] in your \
         content is refused because it is the runtime's hidden-secret \
         placeholder; set allow_redacted_literal only when the user explicitly \
         asked for that literal text."
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

    fn approval_reason(&self, input: &serde_json::Value, context: &ToolContext) -> Option<String> {
        let path = input.get("path")?.as_str()?;
        crate::workspace::path_approval_reason(context, path, true)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;

        // Memory files are persisted by the host policy, which owns their
        // containment, read-before-write snapshot, and manifest refresh. The
        // workspace editor never sees them.
        let memory_path = std::path::PathBuf::from(&input.path);
        let is_memory = match crate::memory_access::preflight_write(
            &context,
            &memory_path,
            input.content.as_bytes(),
        )
        .await
        {
            Ok(is_memory) => is_memory,
            Err(reason) => return Ok(ToolOutput::error(reason)),
        };
        if is_memory {
            let written =
                crate::memory_access::write_file(&context, &memory_path, input.content.as_bytes())
                    .await;
            return match written {
                Ok(leveler_memory::MemoryV2Write::Written { previous_content }) => {
                    let verb = if previous_content.is_some() {
                        "Replaced"
                    } else {
                        "Created"
                    };
                    let lines = input.content.lines().count();
                    let mut meta = serde_json::json!({ "modified_files": [input.path.clone()] });
                    if let Some(previous) = previous_content {
                        let previous = String::from_utf8_lossy(&previous);
                        let hunks =
                            super::applied_diff::whole_file_hunks(&previous, &input.content);
                        if let Some(diff) = super::applied_diff::unified_diff(&input.path, &hunks) {
                            meta["applied_diff"] = serde_json::Value::String(diff);
                        }
                    }
                    Ok(
                        ToolOutput::ok(format!("{verb} {} ({lines} lines)", input.path))
                            .with_metadata(meta),
                    )
                }
                Ok(leveler_memory::MemoryV2Write::Outside) => Ok(ToolOutput::error(
                    "the memory policy accepted this path but did not persist it; \
                     nothing was written",
                )),
                Err(reason) => Ok(ToolOutput::error(reason)),
            };
        }

        if let Some(denied) = context.write_path_denied(&input.path) {
            return Ok(ToolOutput::error(denied));
        }
        let resolved = match context
            .execution_workspace()?
            .resolve_for_write(&input.path, &context.write_scope())
        {
            Ok(resolved) => resolved,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };

        let existing = match tokio::fs::read_to_string(&resolved).await {
            Ok(existing) => Some(existing),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                // A file that exists but is not UTF-8 cannot be compared
                // against, so it cannot be safely overwritten by content.
                return Ok(ToolOutput::error(format!(
                    "cannot read the current contents of {} to replace them: {e}",
                    input.path
                )));
            }
        };

        let commit = match &existing {
            Some(existing) => {
                // The decision to replace the whole file was made against what
                // the model read. If the file moved on since, refuse — exactly
                // as `apply_patch` does.
                if context
                    .execution
                    .file_state
                    .is_stale(&resolved, existing.as_bytes())
                {
                    return Ok(ToolOutput::error(format!(
                        "{} changed since you read it — re-read it and decide again \
                         whether to replace the whole file.",
                        input.path
                    )));
                }
                WorkspaceEditor::replace(
                    &context,
                    &resolved,
                    existing,
                    &input.content,
                    input.allow_redacted_literal,
                )
                .await?
            }
            None => {
                WorkspaceEditor::create(
                    &context,
                    &resolved,
                    &input.content,
                    input.allow_redacted_literal,
                )
                .await?
            }
        };

        match commit {
            Commit::Written(_) => {}
            Commit::Stale => {
                return Ok(ToolOutput::error(format!(
                    "{} changed on disk while this write was being committed — \
                     re-read it and decide again.",
                    input.path
                )));
            }
            Commit::Rejected(message) => return Ok(ToolOutput::error(message)),
        }

        // Re-fingerprint so this write is not mistaken for an outside change.
        context
            .execution
            .file_state
            .record(&resolved, input.content.as_bytes());

        let hunks = super::applied_diff::whole_file_hunks(
            existing.as_deref().unwrap_or(""),
            &input.content,
        );
        let mut meta = serde_json::json!({ "modified_files": [input.path.clone()] });
        if let Some(diff) = super::applied_diff::unified_diff(&input.path, &hunks) {
            meta["applied_diff"] = serde_json::Value::String(diff);
        }
        let verb = if existing.is_some() {
            "Replaced"
        } else {
            "Created"
        };
        let lines = input.content.lines().count();
        Ok(ToolOutput::ok(format!("{verb} {} ({lines} lines)", input.path)).with_metadata(meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn resource_grant_writes_exact_external_file_without_global_authority() {
        use leveler_core::{Capability, GrantBinding, GrantRequest, GrantScope};
        use leveler_execution::{
            AuthorizationEvidence, NetworkScope, PermissionProfile, ResolvedExecutionPolicy,
            WriteScope,
        };
        let (ctx, _root) = super::super::test_ctx(PermissionProfile::Assisted, &[]);
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("approved.txt");
        let other = outside.path().join("other.txt");
        std::fs::write(&path, "original").unwrap();
        std::fs::write(&other, "original-other").unwrap();
        let resource = leveler_execution::resolve_filesystem_resource(&path).unwrap();
        let approved = ctx.with_resolved_policy(ResolvedExecutionPolicy::new(
            WriteScope::None,
            NetworkScope::None,
            AuthorizationEvidence::ResourceGrant {
                request: GrantRequest {
                    project_identity: "p".into(),
                    bindings: vec![GrantBinding {
                        capability: Capability::FilesystemWrite,
                        resource,
                    }],
                },
                scope: GrantScope::Session,
            },
        ));
        let result = run(
            approved.clone(),
            serde_json::json!({"path":path,"content":"approved-change"}),
        )
        .await;
        let other_result = run(
            approved.clone(),
            serde_json::json!({"path":other,"content":"unauthorized-change"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(other_result.is_error);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "approved-change");
        assert_eq!(std::fs::read_to_string(other).unwrap(), "original-other");
        assert!(!approved.policy.unrestricted_execution());
        assert_eq!(approved.policy.network_scope(), NetworkScope::None);
    }

    #[tokio::test]
    async fn sensitive_and_external_file_actions_request_exact_approval() {
        use leveler_execution::{
            AuthorizationEvidence, NetworkScope, PermissionProfile, ResolvedExecutionPolicy,
            WriteScope,
        };
        let (ctx, dir) = super::super::test_ctx(PermissionProfile::Assisted, &[]);
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join(".env");
        let input = serde_json::json!({"path":path,"content":"approved-fixture\n"});
        assert!(WriteFileTool.approval_reason(&input, &ctx).is_some());
        assert!(
            super::super::read_file::ReadFileTool
                .approval_reason(&input, &ctx)
                .is_some()
        );
        assert!(run(ctx.clone(), input.clone()).await.is_error);
        let approved = ctx
            .clone()
            .with_resolved_policy(ResolvedExecutionPolicy::new(
                WriteScope::None,
                NetworkScope::None,
                AuthorizationEvidence::ApprovedOnce,
            ));
        let written = run(approved, input).await;
        assert!(!written.is_error, "{}", written.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "approved-fixture\n"
        );
        assert!(
            WriteFileTool
                .approval_reason(&serde_json::json!({"path":path,"content":"next"}), &ctx)
                .is_some()
        );
        // Best-effort teardown: on Windows the still-open workspace root
        // owns the directory, exactly as its siblings in this module do.
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn full_reads_creates_updates_and_patches_external_credentials() {
        let (ctx, dir) =
            super::super::test_ctx(leveler_execution::PermissionProfile::FullAccess, &[]);
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("nested/.env");
        let create = run(
            ctx.clone(),
            serde_json::json!({"path":path,"content":"before\n"}),
        )
        .await;
        assert!(!create.is_error, "{}", create.content);
        let read = super::super::read_file::ReadFileTool
            .execute(
                serde_json::json!({"path":path}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !read.is_error && read.content.contains("before"),
            "{}",
            read.content
        );
        let update = run(
            ctx.clone(),
            serde_json::json!({"path":path,"content":"after\n"}),
        )
        .await;
        assert!(!update.is_error, "{}", update.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after\n");
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-after\n+patched\n*** End Patch",
            path.display()
        );
        let patched = super::super::apply_patch::ApplyPatchTool
            .execute(
                serde_json::json!({"patch":patch}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!patched.is_error, "{}", patched.content);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "patched\n");
        // Best-effort teardown: on Windows the still-open workspace root
        // owns the directory, exactly as its siblings in this module do.
        std::fs::remove_dir_all(dir).ok();
    }

    /// F-02: the model-visible placeholder cannot be materialized by a whole
    /// file rewrite; an explicit literal request can.
    #[tokio::test]
    async fn placeholder_content_is_refused_unless_explicitly_literal() {
        let (ctx, dir) = super::super::test_ctx(
            leveler_execution::PermissionProfile::Assisted,
            &[("config.txt", "API_TOKEN=hunter2\n")],
        );
        let refused = run(
            ctx.clone(),
            serde_json::json!({"path":"config.txt","content":"API_TOKEN=[REDACTED]\n"}),
        )
        .await;
        assert!(refused.is_error, "{}", refused.content);
        assert!(refused.content.contains("placeholder") || refused.content.contains("REDACTED"));
        assert_eq!(
            std::fs::read_to_string(dir.join("config.txt")).unwrap(),
            "API_TOKEN=hunter2\n"
        );

        let allowed = run(
            ctx,
            serde_json::json!({
                "path":"config.txt",
                "content":"LITERAL=[REDACTED]\n",
                "allow_redacted_literal":true
            }),
        )
        .await;
        assert!(!allowed.is_error, "{}", allowed.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("config.txt")).unwrap(),
            "LITERAL=[REDACTED]\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// INV-03: the default refusal and the literal escape are two different
    /// claims. The refusal protects the ordinary read → rewrite loop; the
    /// escape is a model-controlled integrity opt-in, not a security
    /// boundary. Pinning both keeps the report honest about what the harness
    /// can and cannot prove.
    #[tokio::test]
    async fn placeholder_copied_from_tool_output_is_refused_unless_the_model_claims_literal() {
        let (ctx, dir) = super::super::test_ctx(
            leveler_execution::PermissionProfile::Assisted,
            &[("config.env", "API_TOKEN=hunter2-seed\n")],
        );

        // E: a tool result was sanitized to `[REDACTED]`; the model copies it
        // back. The default guard refuses and the on-disk value survives.
        let copied = run(
            ctx.clone(),
            serde_json::json!({"path":"config.env","content":"API_TOKEN=[REDACTED]\n"}),
        )
        .await;
        assert!(copied.is_error, "{}", copied.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("config.env")).unwrap(),
            "API_TOKEN=hunter2-seed\n"
        );

        // A/C: a new file that merely contains the literal string is refused
        // by the same rule, so the guard does not depend on the target path.
        let created = run(
            ctx.clone(),
            serde_json::json!({"path":"notes.txt","content":"fixture uses [REDACTED]\n"}),
        )
        .await;
        assert!(created.is_error, "{}", created.content);
        assert!(!dir.join("notes.txt").exists());

        // D: when the model sets the escape there is no harness-verifiable
        // user request at this layer; the write is allowed. This is the
        // documented limit, and the call stays visible in the transcript.
        let escaped = run(
            ctx,
            serde_json::json!({
                "path":"config.env",
                "content":"API_TOKEN=[REDACTED]\n",
                "allow_redacted_literal":true
            }),
        )
        .await;
        assert!(!escaped.is_error, "{}", escaped.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("config.env")).unwrap(),
            "API_TOKEN=[REDACTED]\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    async fn run(ctx: ToolContext, args: serde_json::Value) -> ToolOutput {
        WriteFileTool
            .execute(args, ctx, CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_created_file_reports_a_diff_numbered_from_one() {
        let (ctx, dir) =
            super::super::test_ctx(leveler_execution::PermissionProfile::Assisted, &[]);
        let out = run(
            ctx,
            serde_json::json!({"path": "src/new.rs", "content": "a\nb\n"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        let diff = out.metadata["applied_diff"].as_str().expect("applied diff");
        assert!(diff.contains("+a") && diff.contains("+b"), "{diff}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn replacing_a_file_reports_both_sides() {
        let (ctx, dir) = super::super::test_ctx(
            leveler_execution::PermissionProfile::Assisted,
            &[("a.txt", "before\n")],
        );
        let out = run(
            ctx,
            serde_json::json!({"path": "a.txt", "content": "after\n"}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        let diff = out.metadata["applied_diff"].as_str().expect("applied diff");
        assert!(diff.contains("-before"), "{diff}");
        assert!(diff.contains("+after"), "{diff}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_empty_content_is_a_real_empty_file_not_a_refusal() {
        let (ctx, dir) = super::super::test_ctx(
            leveler_execution::PermissionProfile::Assisted,
            &[("a.txt", "before\n")],
        );
        let out = run(ctx, serde_json::json!({"path": "a.txt", "content": ""})).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod alias_identity_tests {
    use super::*;

    #[tokio::test]
    async fn dot_slash_alias_cannot_bypass_the_stale_write_check() {
        let (ctx, dir) = super::super::test_ctx(
            leveler_execution::PermissionProfile::Assisted,
            &[("a.txt", "one\n")],
        );
        crate::tools::read_file::ReadFileTool
            .execute(
                serde_json::json!({"path": "a.txt"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        let out = WriteFileTool
            .execute(
                serde_json::json!({"path": "./a.txt", "content": "replace\n"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{}", out.content);
        assert!(
            out.content.contains("changed since you read it"),
            "the alias must resolve to the same observed resource: {}",
            out.content
        );
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod resume_observation_tests {
    use super::*;

    /// After a resume the tracker is empty. An existing file is an unknown
    /// version, not an untracked one: overwrite requires a fresh read. New
    /// files are still creatable.
    #[tokio::test]
    async fn a_resumed_window_must_read_before_overwriting_an_existing_file() {
        let (ctx, dir) = super::super::test_ctx(
            leveler_execution::PermissionProfile::Assisted,
            &[("a.txt", "old\n")],
        );
        ctx.execution.file_state.set_require_observation(true);
        let refused = WriteFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "content": "new\n"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(refused.is_error, "{}", refused.content);
        assert!(
            refused.content.contains("changed since you read it"),
            "{}",
            refused.content
        );
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "old\n");

        let created = WriteFileTool
            .execute(
                serde_json::json!({"path": "new.txt", "content": "fresh\n"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(
            !created.is_error,
            "a new file has no prior observation to verify: {}",
            created.content
        );

        crate::tools::read_file::ReadFileTool
            .execute(
                serde_json::json!({"path": "a.txt"}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let allowed = WriteFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "content": "new\n"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!allowed.is_error, "{}", allowed.content);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "new\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}
