//! Host-brokered use of credentials the user captured.
//!
//! The model never receives a value. It receives an opaque reference in the
//! user message (`[secret:sec_…]`) and asks this tool to place that value in a
//! configuration file. The tool resolves the reference through the runtime's
//! [`SecretVault`], performs one narrowly specified mutation, and returns a
//! result that names the field but never the value.
//!
//! The first version deliberately supports one target family — dotenv files
//! inside the workspace — and one operation. A general configuration editor
//! would multiply the ways a secret can be written somewhere unintended
//! without making the common task any easier.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_execution::secret_vault::SecretRef;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};
use crate::workspace::{Commit, WorkspaceEditor};

/// Largest dotenv file the broker will edit. A credential file larger than
/// this is not a configuration file a person wrote by hand.
const MAX_ENV_BYTES: u64 = 1024 * 1024;

/// The maximum size of the output file, keeping the growth bounded.
const MAX_ENV_CONTENT: usize = 1024 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
struct ApplyInput {
    /// The captured secret reference, exactly as it appears in the user's
    /// message: `[secret:sec_…]` -> `sec_…`.
    secret_ref: String,
    /// The dotenv field to set, e.g. `DEEPSEEK_API_KEY`.
    field: String,
    /// Dotenv file relative to the workspace. Defaults to `.env`. Only file
    /// names that start with `.env` are accepted.
    #[serde(default = "default_target")]
    target: String,
    /// Only `set_env_key` is supported. Omit it for the default.
    #[serde(default)]
    operation: Option<String>,
}

fn default_target() -> String {
    ".env".to_string()
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InspectInput {
    /// Dotenv file relative to the workspace. Defaults to `.env`.
    #[serde(default = "default_target")]
    target: String,
    /// The field whose presence and value shape should be checked.
    field: String,
}

/// Write one captured secret into a dotenv field.
pub struct SecretApplyTool;

#[async_trait]
impl Tool for SecretApplyTool {
    fn name(&self) -> &'static str {
        "secret_apply"
    }

    fn description(&self) -> &'static str {
        "Write a credential the user captured into a dotenv file in the \
         workspace. The user's message contains it as `[secret:sec_…]`; pass \
         the `sec_…` id as secret_ref. The host resolves the value, updates \
         only the named field, preserves every other line, and returns a \
         result that never contains the value. Use secret_inspect afterwards \
         to confirm the field is present. Do not try to read the value through \
         any other tool; it is not available there."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ApplyInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::WorkspaceWrite
    }

    fn mutates_files(&self) -> bool {
        true
    }

    fn approval_reason(&self, input: &serde_json::Value, context: &ToolContext) -> Option<String> {
        let target = input
            .get("target")
            .and_then(|value| value.as_str())
            .unwrap_or(".env");
        crate::workspace::path_approval_reason(context, target, true)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: ApplyInput = super::parse_input(self.name(), input)?;
        if let Some(operation) = input.operation.as_deref()
            && operation != "set_env_key"
        {
            return Ok(ToolOutput::error(
                "unsupported operation; only set_env_key is available",
            ));
        }
        if !is_env_field(&input.field) {
            return Ok(ToolOutput::error(
                "field must be a dotenv name such as DEEPSEEK_API_KEY \
                 (letters, digits and underscore, not starting with a digit)",
            ));
        }
        let target = match validate_env_target(&input.target) {
            Ok(target) => target,
            Err(reason) => return Ok(ToolOutput::error(reason)),
        };
        let Some(session) = context.session_scope.as_deref() else {
            return Ok(ToolOutput::error(
                "this call has no session; captured secrets are session-scoped",
            ));
        };
        let Some(vault) = context.execution.secret_vault.clone() else {
            return Ok(ToolOutput::error(
                "this runtime holds no captured secrets; ask the user to provide \
                 the credential again with secure capture",
            ));
        };
        let reference = match SecretRef::parse(&input.secret_ref) {
            Ok(reference) => reference,
            Err(_) => {
                return Ok(ToolOutput::error(
                    "secret_ref is not a captured secret reference",
                ));
            }
        };

        if let Some(denied) = context.write_path_denied(&target) {
            return Ok(ToolOutput::error(denied));
        }
        let resolved = match context
            .execution_workspace()?
            .resolve_for_secret_write(&target, &context.write_scope())
        {
            Ok(resolved) => resolved,
            Err(error) => return Ok(ToolOutput::error(error.to_string())),
        };

        let existing = match tokio::fs::metadata(&resolved).await {
            Ok(metadata) => {
                if metadata.len() > MAX_ENV_BYTES {
                    return Ok(ToolOutput::error(format!(
                        "{target} is larger than the broker will edit"
                    )));
                }
                match tokio::fs::read_to_string(&resolved).await {
                    Ok(content) => Some(content),
                    Err(error) => {
                        return Ok(ToolOutput::error(format!("cannot read {target}: {error}")));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Ok(ToolOutput::error(format!("cannot stat {target}: {error}")));
            }
        };

        // Resolve the value only for the duration of rendering the new file.
        let rendered = vault.with_value(session, &reference, |value| {
            if value.contains(['\n', '\r']) {
                return Err("this dotenv writer supports single-line values only");
            }
            Ok(update_env(
                existing.as_deref().unwrap_or_default(),
                &input.field,
                value,
            ))
        });
        let (updated, changed) = match rendered {
            Ok(Ok((updated, changed))) => (zeroize::Zeroizing::new(updated), changed),
            Ok(Err(reason)) => return Ok(ToolOutput::error(reason)),
            Err(error) => return Ok(ToolOutput::error(error.to_string())),
        };
        if updated.len() > MAX_ENV_CONTENT {
            return Ok(ToolOutput::error(
                "the updated file would exceed the broker's size limit",
            ));
        }

        if changed {
            let commit = match &existing {
                Some(existing) => {
                    WorkspaceEditor::replace_secret(&context, &resolved, existing, &updated).await?
                }
                None => {
                    WorkspaceEditor::create_secret(
                        &context,
                        &resolved,
                        &updated,
                        private_permissions(),
                    )
                    .await?
                }
            };
            match commit {
                Commit::Written(_) => {}
                Commit::Stale => {
                    return Ok(ToolOutput::error(format!(
                        "{target} changed on disk while the secret was being written; \
                         nothing was committed. Re-read it and try again."
                    )));
                }
                Commit::Rejected(message) => return Ok(ToolOutput::error(message)),
            }
            context
                .execution
                .file_state
                .record(&resolved, updated.as_bytes());
            // A dotenv that already existed keeps its mode through the atomic
            // replace. Tighten a world-readable one; the value is a credential.
            let tightened = tighten_to_private(&resolved).await;
            vault.mark_used(session, &reference, &target);
            let warning = git_visibility_warning(&context, &target);
            let mut message = format!(
                "Configured {} in {} (the value was written by the host and is not \
                 shown). secret_ref={}",
                input.field, target, input.secret_ref
            );
            if let Some(warning) = warning {
                message.push_str(&format!("\n\nWarning: {warning}"));
            }
            if !tightened {
                message
                    .push_str("\n\nNote: the file's permissions could not be tightened to 0600.");
            }
            return Ok(ToolOutput::ok(message).with_metadata(serde_json::json!({
                "modified_files": [target.clone()],
                "secret_ref": input.secret_ref,
                "field": input.field,
                "value_exposed": false,
                "changed": true,
            })));
        }

        vault.mark_used(session, &reference, &target);
        Ok(ToolOutput::ok(format!(
            "{} already contains the same value for {}; nothing changed. \
             secret_ref={}",
            target, input.field, input.secret_ref
        ))
        .with_metadata(serde_json::json!({
            "modified_files": [],
            "secret_ref": input.secret_ref,
            "field": input.field,
            "value_exposed": false,
            "changed": false,
        })))
    }
}

/// Check that a dotenv field exists, has a non-empty value, and that the file
/// still parses as an assignment list. Never returns the value.
pub struct SecretInspectTool;

#[async_trait]
impl Tool for SecretInspectTool {
    fn name(&self) -> &'static str {
        "secret_inspect"
    }

    fn description(&self) -> &'static str {
        "Check that a dotenv field exists in a workspace file and has a \
         non-empty value, without revealing the value. Use this to verify a \
         secret_apply result."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<InspectInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    fn approval_reason(&self, input: &serde_json::Value, context: &ToolContext) -> Option<String> {
        let target = input
            .get("target")
            .and_then(|value| value.as_str())
            .unwrap_or(".env");
        crate::workspace::path_approval_reason(context, target, false)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: InspectInput = super::parse_input(self.name(), input)?;
        if !is_env_field(&input.field) {
            return Ok(ToolOutput::error(
                "field must be a dotenv name such as DEEPSEEK_API_KEY",
            ));
        }
        let target = match validate_env_target(&input.target) {
            Ok(target) => target,
            Err(reason) => return Ok(ToolOutput::error(reason)),
        };
        let resolved = match context
            .execution_workspace()?
            .resolve_for_secret_read(&target)
        {
            Ok(resolved) => resolved,
            Err(error) => return Ok(ToolOutput::error(error.to_string())),
        };
        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(
                    ToolOutput::ok(format!("{target} does not exist")).with_metadata(
                        serde_json::json!({
                            "target": target,
                            "field": input.field,
                            "present": false,
                            "value_exposed": false,
                        }),
                    ),
                );
            }
            Err(error) => return Ok(ToolOutput::error(format!("cannot read {target}: {error}"))),
        };
        let Some(value) = find_env_value(&content, &input.field) else {
            return Ok(
                ToolOutput::ok(format!("{target} has no {} assignment", input.field))
                    .with_metadata(serde_json::json!({
                        "target": target,
                        "field": input.field,
                        "present": false,
                        "value_exposed": false,
                    })),
            );
        };
        let present = !value.trim().is_empty();
        Ok(ToolOutput::ok(format!(
            "{target} {} {} with a non-empty value",
            if present { "contains" } else { "has" },
            input.field
        ))
        .with_metadata(serde_json::json!({
            "target": target,
            "field": input.field,
            "present": present,
            "value_exposed": false,
        })))
    }
}

// ── dotenv handling ───────────────────────────────────────────────────────

fn is_env_field(field: &str) -> bool {
    let mut chars = field.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_') && field.len() <= 128
}

/// Only dotenv files, relative to the workspace. A nested dotenv
/// (`apps/web/.env`) is allowed; an absolute path, a parent traversal or a
/// different file family is not.
fn validate_env_target(target: &str) -> Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("target must not be empty".into());
    }
    if target.len() > 1024 {
        return Err("target is too long".into());
    }
    if target.starts_with('/') || target.starts_with('\\') || target.contains(':') {
        return Err("target must be relative to the workspace".into());
    }
    if target.split(['/', '\\']).any(|part| part == "..") {
        return Err("target must not leave the workspace".into());
    }
    let file = target.rsplit(['/', '\\']).next().unwrap_or(target);
    if file != ".env" && !file.starts_with(".env.") {
        return Err("only .env files are supported (for example .env or .env.local)".into());
    }
    Ok(target.to_string())
}

/// Set `field` in `content`, preserving every other line and the file's line
/// endings. Returns `(new_content, changed)`.
fn update_env(content: &str, field: &str, value: &str) -> (String, bool) {
    let newline = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let rendered = format!("{field}={}", render_env_value(value));
    let mut out = String::with_capacity(content.len() + rendered.len() + 2);
    let mut replaced = false;
    for line in content.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        match env_field_name(body) {
            Some(name) if name == field => {
                let indent = &body[..body.len() - body.trim_start().len()];
                let export = body.trim_start().starts_with("export ");
                out.push_str(indent);
                if export {
                    out.push_str("export ");
                }
                out.push_str(&rendered);
                if ending.is_empty() && !content.ends_with('\n') {
                    // Preserve a file that deliberately has no final newline.
                } else {
                    out.push_str(if ending.is_empty() { newline } else { ending });
                }
                replaced = true;
            }
            _ => out.push_str(line),
        }
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(newline);
        }
        out.push_str(&rendered);
        out.push_str(newline);
    }
    let changed = out != content;
    (out, changed)
}

fn env_field_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return None;
    }
    let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let (name, _) = trimmed.split_once('=')?;
    let name = name.trim();
    (!name.is_empty()).then_some(name)
}

fn find_env_value<'a>(content: &'a str, field: &str) -> Option<&'a str> {
    for line in content.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if env_field_name(body) != Some(field) {
            continue;
        }
        let trimmed = body.trim_start();
        let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let (_, value) = trimmed.split_once('=')?;
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value);
        return Some(value);
    }
    None
}

/// Render a value so every mainstream dotenv parser reads it back verbatim.
/// Plain tokens stay unquoted; anything else is quoted. A value containing
/// `$` uses single quotes where possible so it is not treated as a variable
/// reference.
fn render_env_value(value: &str) -> String {
    let plain = !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '+' | '=')
        });
    if plain {
        return value.to_string();
    }
    if value.contains('$') && !value.contains('\'') {
        return format!("'{value}'");
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(unix)]
fn private_permissions() -> Option<std::fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    Some(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn private_permissions() -> Option<std::fs::Permissions> {
    None
}

#[cfg(unix)]
async fn tighten_to_private(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return false;
    };
    if metadata.permissions().mode() & 0o077 == 0 {
        return true;
    }
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .await
        .is_ok()
}

#[cfg(not(unix))]
async fn tighten_to_private(_path: &std::path::Path) -> bool {
    true
}

/// Warn when a dotenv is inside a Git repository and not ignored by the local
/// `.gitignore`. This is a visibility hint, not an authorization decision: the
/// broker cannot prove what a future commit will include.
fn git_visibility_warning(context: &ToolContext, target: &str) -> Option<String> {
    let workspace = context.execution_workspace().ok()?;
    let root = workspace.root();
    if !root.join(".git").exists() {
        return None;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    let matcher = builder.build().ok()?;
    let relative = std::path::Path::new(target);
    if matcher
        .matched_path_or_any_parents(relative, false)
        .is_ignore()
    {
        return None;
    }
    Some(format!(
        "{target} is not ignored by .gitignore; make sure the credential cannot be \
         committed"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_execution::PermissionProfile;
    use leveler_execution::secret_vault::SecretVault;
    use std::sync::Arc;

    fn broker_ctx(
        files: &[(&str, &str)],
    ) -> (ToolContext, std::path::PathBuf, Arc<SecretVault>, SecretRef) {
        let (context, dir) = super::super::test_ctx(PermissionProfile::Assisted, files);
        let vault = Arc::new(SecretVault::new());
        let reference = vault
            .capture("s", "sk-2f8a1c9e4b7d6a5f3e2c1b0a".into(), Some("K".into()))
            .unwrap();
        let context = context
            .with_session_scope("s")
            .with_secret_vault(vault.clone());
        (context, dir, vault, reference)
    }

    async fn apply(
        context: &ToolContext,
        reference: &SecretRef,
        field: &str,
        target: &str,
    ) -> ToolOutput {
        SecretApplyTool
            .execute(
                serde_json::json!({
                    "secret_ref": reference.as_str(),
                    "field": field,
                    "target": target,
                }),
                context.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn creates_a_dotenv_with_the_real_value_and_never_prints_it() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        let output = apply(&context, &reference, "DEEPSEEK_API_KEY", ".env").await;
        assert!(!output.is_error, "{}", output.content);
        let written = std::fs::read_to_string(dir.join(".env")).unwrap();
        assert_eq!(written, "DEEPSEEK_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a\n");
        assert!(
            !output.content.contains("sk-2f8a1c9e"),
            "value leaked into the model result: {}",
            output.content
        );
        assert!(!output.metadata.to_string().contains("sk-2f8a1c9e"));
        assert_eq!(output.metadata["value_exposed"], false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(".env"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "dotenv must not be group/world readable");
        }
    }

    #[tokio::test]
    async fn updates_one_line_and_preserves_everything_else_and_crlf() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        let original = "# keep me\r\nDEEPSEEK_API_KEY=old-value-1\r\nOTHER=visible\r\nexport EXPORTED=old-2\r\n";
        std::fs::write(dir.join(".env"), original).unwrap();
        let output = apply(&context, &reference, "DEEPSEEK_API_KEY", ".env").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "# keep me\r\nDEEPSEEK_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a\r\nOTHER=visible\r\nexport EXPORTED=old-2\r\n"
        );

        let output = apply(&context, &reference, "EXPORTED", ".env").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).unwrap(),
            "# keep me\r\nDEEPSEEK_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a\r\nOTHER=visible\r\nexport EXPORTED=sk-2f8a1c9e4b7d6a5f3e2c1b0a\r\n"
        );
    }

    #[tokio::test]
    async fn a_second_identical_apply_is_idempotent() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        let first = apply(&context, &reference, "API_KEY", ".env").await;
        assert!(!first.is_error);
        let bytes = std::fs::read(dir.join(".env")).unwrap();
        let second = apply(&context, &reference, "API_KEY", ".env").await;
        assert!(!second.is_error);
        assert_eq!(second.metadata["changed"], false);
        assert_eq!(std::fs::read(dir.join(".env")).unwrap(), bytes);
    }

    #[tokio::test]
    async fn a_reference_from_another_session_is_refused_without_side_effects() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        let other = context.clone().with_session_scope("other");
        let output = apply(&other, &reference, "API_KEY", ".env").await;
        assert!(output.is_error);
        assert!(
            !dir.join(".env").exists(),
            "a refused apply must write nothing"
        );
        assert!(!output.content.contains("sk-2f8a1c9e"));
    }

    #[tokio::test]
    async fn malformed_refs_fields_and_targets_are_refused() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        for (field, target) in [
            ("1BAD", ".env"),
            ("BAD-NAME", ".env"),
            ("OK", "config.toml"),
            ("OK", "../.env"),
            ("OK", "/tmp/.env"),
        ] {
            let output = apply(&context, &reference, field, target).await;
            assert!(output.is_error, "{field} {target} should be refused");
        }
        let output = SecretApplyTool
            .execute(
                serde_json::json!({
                    "secret_ref": "sec_00000000000000000000000000000000",
                    "field": "OK",
                    "target": ".env",
                }),
                context.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(!dir.join(".env").exists());
    }

    #[tokio::test]
    async fn a_chinese_path_with_spaces_is_supported() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        std::fs::create_dir_all(dir.join("配置 目录")).unwrap();
        let output = apply(&context, &reference, "API_KEY", "配置 目录/.env").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("配置 目录/.env")).unwrap(),
            "API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a\n"
        );
    }

    #[tokio::test]
    async fn a_multiline_value_is_refused_instead_of_corrupting_the_dotenv() {
        let (context, dir) = super::super::test_ctx(PermissionProfile::Assisted, &[]);
        let vault = Arc::new(SecretVault::new());
        let reference = vault
            .capture(
                "s",
                "-----BEGIN KEY-----\nabc\n-----END KEY-----".into(),
                None,
            )
            .unwrap();
        let context = context.with_session_scope("s").with_secret_vault(vault);
        let output = apply(&context, &reference, "PRIVATE_KEY", ".env").await;
        assert!(output.is_error);
        assert!(output.content.contains("single-line"), "{}", output.content);
        assert!(!dir.join(".env").exists(), "nothing was written");
    }

    #[tokio::test]
    async fn expired_secrets_are_refused() {
        let (context, _dir, vault, reference) = broker_ctx(&[]);
        vault.revoke("s", &reference);
        let output = apply(&context, &reference, "API_KEY", ".env").await;
        assert!(output.is_error);
        assert!(output.content.contains("revoked"), "{}", output.content);
    }

    #[tokio::test]
    async fn quoting_round_trips_through_the_inspector() {
        let (context, dir) = super::super::test_ctx(PermissionProfile::Assisted, &[]);
        let vault = Arc::new(SecretVault::new());
        let reference = vault
            .capture("s", "pa$$ word with \"quotes\"".into(), None)
            .unwrap();
        let context = context.with_session_scope("s").with_secret_vault(vault);
        let output = apply(&context, &reference, "PASSWORD", ".env").await;
        assert!(!output.is_error, "{}", output.content);
        let written = std::fs::read_to_string(dir.join(".env")).unwrap();
        assert!(written.contains("PASSWORD="), "{written}");
        assert!(!written.contains("pa$$ word with \"quotes\"") || written.contains("'"));
        let inspected = SecretInspectTool
            .execute(
                serde_json::json!({ "field": "PASSWORD" }),
                context.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!inspected.is_error);
        assert_eq!(inspected.metadata["present"], true);
        assert!(!inspected.metadata.to_string().contains("pa$$"));
    }

    #[tokio::test]
    async fn a_symlink_that_leaves_the_workspace_is_refused() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = tempfile::tempdir().unwrap();
            let secret_file = outside.path().join("outside.env");
            std::fs::write(&secret_file, "UNCHANGED=1\n").unwrap();
            let (context, dir) = super::super::test_ctx(PermissionProfile::Assisted, &[]);
            symlink(&secret_file, dir.join(".env")).unwrap();
            let vault = Arc::new(SecretVault::new());
            let reference = vault
                .capture("s", "sk-2f8a1c9e4b7d6a5f3e2c1b0a".into(), None)
                .unwrap();
            let context = context.with_session_scope("s").with_secret_vault(vault);
            let output = apply(&context, &reference, "API_KEY", ".env").await;
            assert!(output.is_error, "{}", output.content);
            assert_eq!(
                std::fs::read_to_string(&secret_file).unwrap(),
                "UNCHANGED=1\n",
                "the symlink target must not be modified"
            );
        }
    }

    #[test]
    fn the_transcript_token_round_trips() {
        let reference = SecretRef::parse("sec_0123456789abcdef0123456789abcdef").unwrap();
        let token = leveler_core::secret_reference_token(reference.as_str());
        assert_eq!(token, "[secret:sec_0123456789abcdef0123456789abcdef]");
        assert_eq!(
            leveler_core::parse_secret_reference_token(&token),
            Some(reference.as_str())
        );
        assert!(leveler_core::parse_secret_reference_token("[secret:not-a-ref]").is_none());
    }
}
