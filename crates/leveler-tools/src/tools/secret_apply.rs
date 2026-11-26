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

    fn exact_call_consent(&self) -> bool {
        // One approval authorizes one captured value for one target and field.
        // It can never become a tool-wide rule: the authority lives in the
        // vault entry, not in a permission rule.
        true
    }

    fn approval_reason(&self, input: &serde_json::Value, context: &ToolContext) -> Option<String> {
        // Full never asks. Every other mode asks only when the user's own
        // message did not already name this exact use: an over-scope request
        // is refused by the tool, not put to the person as a question the
        // tool would then reject anyway.
        if context.policy.unrestricted_execution() {
            return None;
        }
        let target = input
            .get("target")
            .and_then(|value| value.as_str())
            .unwrap_or(".env");
        let target = leveler_core::normalize_dotenv_target(target)?;
        let field = input.get("field")?.as_str()?;
        if !is_env_field(field) {
            return None;
        }
        let reference = SecretRef::parse(input.get("secret_ref")?.as_str()?).ok()?;
        let session = context.session_scope.as_deref()?;
        let vault = context.execution.secret_vault.as_ref()?;
        let scope = vault.use_scope(session, &reference).ok()?;
        if hard_scope_refusal(&scope, &target, field).is_some() {
            return None;
        }
        // The person already approved this exact use. A second identical call
        // is the same authorized operation, not a new question.
        if scope
            .approved_uses
            .iter()
            .any(|approved| approved.target == target && approved.field == field)
        {
            return None;
        }
        // No prompt only when the person's own message named BOTH dimensions.
        // A target without a field (or a field without a target) leaves the
        // other half to the model, which is exactly the sensitive credential
        // use Auto asks about.
        let unbound_dimensions = match (scope.targets.is_empty(), scope.fields.is_empty()) {
            (false, false) => "",
            (true, true) => "目标与字段都未在用户消息中声明",
            (true, false) => "目标未在用户消息中声明",
            (false, true) => "字段未在用户消息中声明",
        };
        // An unattributed mention is not a missing dimension the model may
        // fill in: the user said something that could name this dimension, but
        // the message did not bind it to this reference. Guess and the write
        // may land somewhere the user did not authorize.
        let unattributed = scope.ambiguous && (scope.targets.is_empty() || scope.fields.is_empty());
        if unbound_dimensions.is_empty() && !unattributed {
            return None;
        }
        // Do not prompt for a write the execution layer would refuse.
        if context.write_path_denied(&target).is_some() {
            return None;
        }
        context
            .execution_workspace()
            .ok()?
            .resolve_for_secret_write(&target, &context.write_scope())
            .ok()?;
        let reason = if unattributed {
            format!(
                "用户消息提到了一个无法唯一归属到该凭据的 .env 目标或字段。\
                 使用已接管凭据（{}）写入 {target} 的 {field} 需要你确认这次精确使用。",
                short_reference(&reference)
            )
        } else {
            format!(
                "使用已接管凭据（{}）写入 {target} 的 {field}。{unbound_dimensions}，请确认。",
                short_reference(&reference)
            )
        };
        Some(reason)
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
        // The person's own messages are the use authorization. A message that
        // named `.env` and `DEMO_API_KEY` permits exactly that; it does not
        // permit the model to choose another target or field afterwards.
        let scope = match vault.use_scope(session, &reference) {
            Ok(scope) => scope,
            Err(error) => return Ok(ToolOutput::error(error.to_string())),
        };
        if let Some(reason) = scope_refusal(&scope, &target, &input.field) {
            return Ok(ToolOutput::error(reason));
        }

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
        // The value is already in the file; an existing world-readable file
        // must still be tightened. The commit path tightens before returning,
        // but an idempotent retry never reaches it.
        let tightened = tighten_to_private(&resolved).await;
        let mut message = format!(
            "{} already contains the same value for {}; nothing changed. \
             secret_ref={}",
            target, input.field, input.secret_ref
        );
        if !tightened {
            message.push_str("\n\nNote: the file's permissions could not be tightened to 0600.");
        }
        Ok(ToolOutput::ok(message).with_metadata(serde_json::json!({
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
    leveler_core::normalize_dotenv_target(target).ok_or_else(|| {
        "only .env files are supported, relative to the workspace (for example .env or .env.local)"
            .to_string()
    })
}

/// A refusal that no approval can lift: an explicit prohibition, an exact use
/// the person refused, or a use outside a non-empty declared dimension.
fn hard_scope_refusal(
    scope: &leveler_execution::secret_vault::SecretUseScope,
    target: &str,
    field: &str,
) -> Option<String> {
    if scope
        .denied_uses
        .iter()
        .any(|denied| denied.target == target && denied.field == field)
    {
        return Some(format!(
            "the user refused this exact use of the captured secret ({target}, {field}). \
             Do not retry it; ask the user whether a different use is intended."
        ));
    }
    if scope.denied_targets.iter().any(|denied| denied == target) {
        return Some(format!(
            "the user explicitly prohibited writing this captured secret to {target}. \
             Do not use that target; ask the user which target they authorize."
        ));
    }
    if scope.denied_fields.iter().any(|denied| denied == field) {
        return Some(format!(
            "the user explicitly prohibited writing this captured secret to field {field}. \
             Do not use that field; ask the user which field they authorize."
        ));
    }
    if !scope.targets.is_empty() && !scope.targets.iter().any(|allowed| allowed == target) {
        return Some(format!(
            "this captured secret is authorized only for {}; the user's message did not \
             name {target}. Tell the user which target is needed and let them state it \
             explicitly; do not use another target.",
            scope.targets.join(", ")
        ));
    }
    if !scope.fields.is_empty() && !scope.fields.iter().any(|allowed| allowed == field) {
        return Some(format!(
            "this captured secret is authorized only for field {}; the user's message did \
             not name {field}. Tell the user which field is needed and let them state it \
             explicitly; do not use another field.",
            scope.fields.join(", ")
        ));
    }
    None
}

/// Whether the exact use is outside what the person's own message authorized.
///
/// The authorization unit is the exact `(reference, target, field)` use. A
/// denial is absolute: an explicit prohibition and an exact use the person
/// refused are both enforced in every permission mode, including Full. An
/// empty dimension is not a refusal by itself — the person may still approve
/// that use — but a dimension the message mentioned without binding it to this
/// reference must never be guessed.
fn scope_refusal(
    scope: &leveler_execution::secret_vault::SecretUseScope,
    target: &str,
    field: &str,
) -> Option<String> {
    if let Some(reason) = hard_scope_refusal(scope, target, field) {
        return Some(reason);
    }
    // A mention exists that could not be attributed to this reference. With an
    // empty dimension there is nothing to check the model's choice against, so
    // only an exact approval recorded through the ordinary consent surface can
    // make this use concrete. This also holds in Full: the mode removes
    // permission friction, not the user's authority over their own credential.
    if scope.ambiguous
        && (scope.targets.is_empty() || scope.fields.is_empty())
        && !scope
            .approved_uses
            .iter()
            .any(|approved| approved.target == target && approved.field == field)
    {
        return Some(format!(
            "the user's message named a .env target or field it did not bind to this \
             captured secret. Writing {field} to {target} would guess; ask the user to \
             state the exact target and field for this reference."
        ));
    }
    None
}

/// Record a person's exact-call decision for a credential write.
///
/// The approval unit is `(reference, target, field)`, never the tool or the
/// reference alone. Returns whether the decision applied to a live capture in
/// this session; `false` means the call was not a credential write or the
/// reference was already unavailable.
pub fn record_secret_call_decision(
    context: &ToolContext,
    tool: &str,
    input: &serde_json::Value,
    approved: bool,
) -> bool {
    if tool != "secret_apply" {
        return false;
    }
    let Some(session) = context.session_scope.as_deref() else {
        return false;
    };
    let Some(vault) = context.execution.secret_vault.as_deref() else {
        return false;
    };
    let Some(reference) = input
        .get("secret_ref")
        .and_then(|value| value.as_str())
        .and_then(|value| SecretRef::parse(value).ok())
    else {
        return false;
    };
    let Some(target) = leveler_core::normalize_dotenv_target(
        input
            .get("target")
            .and_then(|value| value.as_str())
            .unwrap_or(".env"),
    ) else {
        return false;
    };
    let Some(field) = input.get("field").and_then(|value| value.as_str()) else {
        return false;
    };
    if !is_env_field(field) {
        return false;
    }
    if approved {
        vault.record_exact_approval(session, &reference, &target, field)
    } else {
        vault.record_exact_denial(session, &reference, &target, field)
    }
}

/// A reference suffix safe to show in an approval prompt: the model already
/// has the full reference, and the suffix is enough to tell two prompts apart.
fn short_reference(reference: &SecretRef) -> String {
    let text = reference.as_str();
    format!("sec_…{}", &text[text.len().saturating_sub(6)..])
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
async fn tighten_to_private(path: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        // Windows has no mode bits. The equivalent of 0600 is an owner-only,
        // inheritance-protected DACL: only the current user, SYSTEM and the
        // Administrators group. Applying it after the atomic replace leaves a
        // short window that the sandbox/permission mode already covers; a
        // file the host created earlier is tightened here too.
        leveler_win_confine::secure_private_file(path).is_ok()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        true
    }
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
    async fn an_unchanged_existing_dotenv_is_still_tightened() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let (context, dir, _vault, reference) = broker_ctx(&[]);
            let path = dir.join(".env");
            std::fs::write(&path, "API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let output = apply(&context, &reference, "API_KEY", ".env").await;
            assert!(!output.is_error, "{}", output.content);
            assert_eq!(output.metadata["changed"], false);
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o077,
                0,
                "an idempotent retry must still tighten the credential file"
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn the_dotenv_gets_an_owner_only_acl() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        let output = apply(&context, &reference, "DEMO_API_KEY", ".env").await;
        assert!(!output.is_error, "{}", output.content);
        leveler_win_confine::verify_private_state_file(&dir.join(".env"))
            .expect("the credential file must carry the owner-only DACL");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn an_unchanged_existing_dotenv_is_still_protected() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        let path = dir.join(".env");
        std::fs::write(&path, "API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a\n").unwrap();
        let output = apply(&context, &reference, "API_KEY", ".env").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.metadata["changed"], false);
        leveler_win_confine::verify_private_state_file(&path)
            .expect("an idempotent retry must still protect the credential file");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_new_dotenv_in_a_broad_directory_is_private_from_the_start() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        let status = std::process::Command::new("icacls")
            .arg(&dir)
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .status()
            .unwrap();
        assert!(status.success());
        let output = apply(&context, &reference, "DEMO_API_KEY", ".env").await;
        assert!(!output.is_error, "{}", output.content);
        let path = dir.join(".env");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "DEMO_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a\n"
        );
        leveler_win_confine::verify_private_state_file(&path)
            .expect("a new credential file must not inherit the directory's broad DACL");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_changed_write_tightens_a_broad_existing_dotenv_without_leaving_a_stage() {
        let (context, dir, _vault, reference) = broker_ctx(&[]);
        let path = dir.join(".env");
        std::fs::write(&path, "API_KEY=old-value-1\nOTHER=visible\n").unwrap();
        let status = std::process::Command::new("icacls")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:R"])
            .status()
            .unwrap();
        assert!(status.success());
        leveler_win_confine::verify_private_state_file(&path)
            .expect_err("the probe file must start broad");

        let output = apply(&context, &reference, "API_KEY", ".env").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a\nOTHER=visible\n"
        );
        leveler_win_confine::verify_private_state_file(&path)
            .expect("a changed credential write must leave a private file");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".leveler-tmp")
            })
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
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

    #[tokio::test]
    async fn a_child_execution_shares_the_capture_scope_but_not_extra_authority() {
        let (context, dir, vault, reference) = broker_ctx(&[]);
        assert!(vault.authorize_use(
            "s",
            &reference,
            &[".env".to_string()],
            &["DEMO_API_KEY".to_string()]
        ));
        let child = context
            .clone()
            .with_writer_scope("child-writer")
            .with_tool_call_scope("child-call-2");
        let inside = apply(&child, &reference, "DEMO_API_KEY", ".env").await;
        assert!(!inside.is_error, "{}", inside.content);
        let outside = apply(&child, &reference, "DEMO_API_KEY", ".env.production").await;
        assert!(
            outside.is_error,
            "a delegated writer must not widen the capture's use"
        );
        assert!(!dir.join(".env.production").exists());
    }

    #[tokio::test]
    async fn an_apply_outside_the_users_declared_target_is_refused_in_every_mode() {
        for mode in [
            PermissionProfile::Assisted,
            PermissionProfile::FullAccess,
            PermissionProfile::RequestApproval,
        ] {
            let (context, dir) = super::super::test_ctx(mode, &[]);
            let vault = Arc::new(SecretVault::new());
            let reference = vault
                .capture("s", "sk-2f8a1c9e4b7d6a5f3e2c1b0a".into(), None)
                .unwrap();
            assert!(vault.authorize_use(
                "s",
                &reference,
                &[".env".to_string()],
                &["DEMO_API_KEY".to_string()]
            ));
            let context = context.with_session_scope("s").with_secret_vault(vault);

            let authorized = apply(&context, &reference, "DEMO_API_KEY", ".env").await;
            assert!(!authorized.is_error, "{mode:?}: {}", authorized.content);

            let other_target = apply(&context, &reference, "DEMO_API_KEY", ".env.production").await;
            assert!(
                other_target.is_error,
                "{mode:?}: a new target must be refused"
            );
            assert!(
                !dir.join(".env.production").exists(),
                "{mode:?}: a refused target writes nothing"
            );

            let other_field = apply(&context, &reference, "ATTACKER_TOKEN", ".env").await;
            assert!(
                other_field.is_error,
                "{mode:?}: a new field must be refused"
            );
            assert_eq!(
                std::fs::read_to_string(dir.join(".env")).unwrap(),
                "DEMO_API_KEY=sk-2f8a1c9e4b7d6a5f3e2c1b0a\n",
                "{mode:?}: the authorized write is the only one that landed"
            );
            assert!(!other_field.content.contains("sk-2f8a1c9e"));
        }
    }

    #[tokio::test]
    async fn an_apply_inside_the_declared_scope_needs_no_further_approval() {
        let (context, dir, vault, reference) = broker_ctx(&[]);
        assert!(vault.authorize_use(
            "s",
            &reference,
            &[".env".to_string()],
            &["DEMO_API_KEY".to_string()]
        ));
        let input = serde_json::json!({
            "secret_ref": reference.as_str(),
            "field": "DEMO_API_KEY",
            "target": ".env",
        });
        assert_eq!(
            SecretApplyTool.approval_reason(&input, &context),
            None,
            "the user's own message already authorized this exact use"
        );
        let output = SecretApplyTool
            .execute(input, context.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert!(!output.is_error, "{}", output.content);
        assert!(dir.join(".env").exists());
    }

    #[tokio::test]
    async fn a_partially_declared_use_asks_for_the_missing_dimension() {
        let (context, _dir, vault, reference) = broker_ctx(&[]);
        assert!(vault.authorize_use("s", &reference, &[".env".to_string()], &[]));
        let input = serde_json::json!({
            "secret_ref": reference.as_str(),
            "field": "DEMO_API_KEY",
            "target": ".env",
        });
        let reason = SecretApplyTool
            .approval_reason(&input, &context)
            .expect("a target without a field is not a full use authorization");
        assert!(reason.contains("字段未在用户消息中声明"), "{reason}");

        let (context, _dir, vault, reference) = broker_ctx(&[]);
        assert!(vault.authorize_use("s", &reference, &[], &["DEMO_API_KEY".to_string()]));
        let input = serde_json::json!({
            "secret_ref": reference.as_str(),
            "field": "DEMO_API_KEY",
            "target": ".env",
        });
        let reason = SecretApplyTool
            .approval_reason(&input, &context)
            .expect("a field without a target is not a full use authorization");
        assert!(reason.contains("目标未在用户消息中声明"), "{reason}");
    }

    #[tokio::test]
    async fn an_unscoped_reference_asks_and_an_out_of_scope_one_does_not() {
        let (context, _dir, vault, reference) = broker_ctx(&[]);
        let input = serde_json::json!({
            "secret_ref": reference.as_str(),
            "field": "DEMO_API_KEY",
            "target": ".env",
        });
        let reason = SecretApplyTool
            .approval_reason(&input, &context)
            .expect("an unbound credential use is a decision for the person");
        assert!(reason.contains(".env"), "{reason}");
        assert!(reason.contains("DEMO_API_KEY"), "{reason}");
        assert!(!reason.contains("sk-2f8a1c9e"), "{reason}");
        assert!(
            SecretApplyTool.exact_call_consent(),
            "one approval must not become a standing credential-use rule"
        );

        assert!(vault.authorize_use(
            "s",
            &reference,
            &[".env".to_string()],
            &["DEMO_API_KEY".to_string()]
        ));
        let other = serde_json::json!({
            "secret_ref": reference.as_str(),
            "field": "DEMO_API_KEY",
            "target": ".env.production",
        });
        assert_eq!(
            SecretApplyTool.approval_reason(&other, &context),
            None,
            "an over-scope use is refused by the broker, not put to the person as a question"
        );

        let full = context.clone().with_permission_profile(
            leveler_execution::SharedPermissionProfile::new(PermissionProfile::FullAccess),
        );
        assert_eq!(
            SecretApplyTool.approval_reason(&input, &full),
            None,
            "Full mode never asks"
        );
    }

    #[tokio::test]
    async fn a_prohibited_target_is_refused_in_every_mode() {
        for mode in [
            PermissionProfile::Assisted,
            PermissionProfile::FullAccess,
            PermissionProfile::RequestApproval,
        ] {
            let (context, dir, vault, reference) = broker_ctx(&[]);
            assert!(vault.authorize_declaration(
                "s",
                &reference,
                &leveler_core::DotenvUseDeclaration {
                    targets: vec![".env".to_string()],
                    fields: vec!["DEMO_API_KEY".to_string()],
                    denied_targets: vec![".env.production".to_string()],
                    ..Default::default()
                }
            ));
            let context = context
                .with_permission_profile(leveler_execution::SharedPermissionProfile::new(mode));
            let denied = apply(&context, &reference, "DEMO_API_KEY", ".env.production").await;
            assert!(denied.is_error, "{mode:?}: {}", denied.content);
            assert!(denied.content.contains("prohibited"), "{}", denied.content);
            assert!(
                !dir.join(".env.production").exists(),
                "{mode:?}: a prohibited target writes nothing"
            );

            let allowed = apply(&context, &reference, "DEMO_API_KEY", ".env").await;
            assert!(!allowed.is_error, "{mode:?}: {}", allowed.content);
        }
    }

    #[tokio::test]
    async fn an_ambiguous_unbound_use_is_not_guessed() {
        for mode in [
            PermissionProfile::Assisted,
            PermissionProfile::FullAccess,
            PermissionProfile::RequestApproval,
        ] {
            let (context, dir, vault, reference) = broker_ctx(&[]);
            assert!(vault.authorize_declaration(
                "s",
                &reference,
                &leveler_core::DotenvUseDeclaration {
                    ambiguous: true,
                    ..Default::default()
                }
            ));
            let context = context
                .with_permission_profile(leveler_execution::SharedPermissionProfile::new(mode));
            let input = serde_json::json!({
                "secret_ref": reference.as_str(),
                "field": "DEMO_API_KEY",
                "target": ".env",
            });
            let reason = SecretApplyTool.approval_reason(&input, &context);
            if mode == PermissionProfile::FullAccess {
                assert_eq!(reason, None, "Full mode never asks");
            } else {
                let reason = reason.expect("an unattributed dimension is a question");
                assert!(reason.contains("无法唯一归属"), "{reason}");
            }
            let output = apply(&context, &reference, "DEMO_API_KEY", ".env").await;
            assert!(output.is_error, "{mode:?}: {}", output.content);
            assert!(
                output.content.contains("did not bind"),
                "{}",
                output.content
            );
            assert!(!dir.join(".env").exists(), "{mode:?}: nothing was written");

            // The ordinary consent surface records this exact use; the same
            // call then becomes concrete and is allowed.
            assert!(record_secret_call_decision(
                &context,
                "secret_apply",
                &input,
                true
            ));
            let output = apply(&context, &reference, "DEMO_API_KEY", ".env").await;
            assert!(!output.is_error, "{mode:?}: {}", output.content);
            assert!(dir.join(".env").exists(), "{mode:?}");

            // The approval binds to the exact use only.
            let other = apply(&context, &reference, "OTHER_KEY", ".env").await;
            assert!(other.is_error, "{mode:?}: a new field is a new question");
        }
    }

    #[tokio::test]
    async fn an_approved_exact_use_is_not_asked_again() {
        let (context, _dir, vault, reference) = broker_ctx(&[]);
        assert!(vault.authorize_declaration(
            "s",
            &reference,
            &leveler_core::DotenvUseDeclaration {
                ambiguous: true,
                ..Default::default()
            }
        ));
        let input = serde_json::json!({
            "secret_ref": reference.as_str(),
            "field": "DEMO_API_KEY",
            "target": ".env",
        });
        assert!(
            SecretApplyTool.approval_reason(&input, &context).is_some(),
            "an unbound use is a question"
        );
        assert!(record_secret_call_decision(
            &context,
            "secret_apply",
            &input,
            true
        ));
        assert_eq!(
            SecretApplyTool.approval_reason(&input, &context),
            None,
            "the person already approved this exact use"
        );
        let output = apply(&context, &reference, "DEMO_API_KEY", ".env").await;
        assert!(!output.is_error, "{}", output.content);
    }

    #[tokio::test]
    async fn a_refused_exact_use_stays_refused_after_the_mode_changes() {
        let (context, dir, vault, reference) = broker_ctx(&[]);
        // The person named the target and field, so the first call needs no
        // broker question under Assisted.
        assert!(vault.authorize_use(
            "s",
            &reference,
            &[".env".to_string()],
            &["DEMO_API_KEY".to_string()]
        ));
        let input = serde_json::json!({
            "secret_ref": reference.as_str(),
            "field": "DEMO_API_KEY",
            "target": ".env",
        });
        assert_eq!(SecretApplyTool.approval_reason(&input, &context), None);
        assert!(record_secret_call_decision(
            &context,
            "secret_apply",
            &input,
            false
        ));

        let full = context.clone().with_permission_profile(
            leveler_execution::SharedPermissionProfile::new(PermissionProfile::FullAccess),
        );
        assert_eq!(
            SecretApplyTool.approval_reason(&input, &full),
            None,
            "a denied exact use is refused, not asked again"
        );
        let output = apply(&full, &reference, "DEMO_API_KEY", ".env").await;
        assert!(output.is_error, "{}", output.content);
        assert!(output.content.contains("refused"), "{}", output.content);
        assert!(!dir.join(".env").exists(), "a refusal writes nothing");

        // A later bare mention does NOT lift the refusal. Only a new capture
        // (or a fresh approval through the consent surface) is a new decision.
        assert!(vault.authorize_use(
            "s",
            &reference,
            &[".env".to_string()],
            &["DEMO_API_KEY".to_string()]
        ));
        let output = apply(&full, &reference, "DEMO_API_KEY", ".env").await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            !dir.join(".env").exists(),
            "a refused use stays refused without a new decision"
        );

        let fresh = vault
            .capture("s", "sk-2f8a1c9e4b7d6a5f3e2c1b0a".into(), None)
            .unwrap();
        assert!(vault.authorize_use(
            "s",
            &fresh,
            &[".env".to_string()],
            &["DEMO_API_KEY".to_string()]
        ));
        let output = apply(&full, &fresh, "DEMO_API_KEY", ".env").await;
        assert!(!output.is_error, "{}", output.content);
        assert!(dir.join(".env").exists());
    }

    #[tokio::test]
    async fn two_references_do_not_share_a_target_grant() {
        let (context, dir, vault, _reference) = broker_ctx(&[]);
        let first = vault
            .capture("s", "sk-first-2f8a1c9e4b7d6a5f".into(), None)
            .unwrap();
        let second = vault
            .capture("s", "sk-second-2f8a1c9e4b7d6a5f".into(), None)
            .unwrap();
        assert!(vault.authorize_use("s", &first, &[".env".to_string()], &["A_KEY".to_string()]));
        assert!(vault.authorize_use(
            "s",
            &second,
            &[".env.production".to_string()],
            &["B_KEY".to_string()]
        ));
        let cross = apply(&context, &first, "B_KEY", ".env.production").await;
        assert!(cross.is_error, "{}", cross.content);
        assert!(!dir.join(".env.production").exists());

        let own = apply(&context, &second, "B_KEY", ".env.production").await;
        assert!(!own.is_error, "{}", own.content);
        assert_eq!(
            std::fs::read_to_string(dir.join(".env.production")).unwrap(),
            "B_KEY=sk-second-2f8a1c9e4b7d6a5f\n"
        );
    }

    #[tokio::test]
    async fn two_concurrent_writes_leave_one_complete_dotenv() {
        let (context, dir, vault, reference) = broker_ctx(&[]);
        assert!(vault.authorize_use(
            "s",
            &reference,
            &[".env".to_string()],
            &["A_KEY".to_string(), "B_KEY".to_string()]
        ));
        let first = apply(&context, &reference, "A_KEY", ".env");
        let second = apply(&context, &reference, "B_KEY", ".env");
        let (first, second) = tokio::join!(first, second);
        assert!(
            !first.is_error || !second.is_error,
            "at least one serialized write must commit: {:?} / {:?}",
            first.content,
            second.content
        );
        let written = std::fs::read_to_string(dir.join(".env")).unwrap();
        for line in written.lines() {
            assert!(
                line.starts_with("A_KEY=") || line.starts_with("B_KEY="),
                "a torn line was committed: {written}"
            );
        }
        assert!(written.contains("sk-2f8a1c9e"), "{written}");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".leveler-tmp")
            })
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn recording_a_decision_ignores_calls_that_are_not_credential_writes() {
        let (context, _dir, _vault, reference) = broker_ctx(&[]);
        let input = serde_json::json!({
            "secret_ref": reference.as_str(),
            "field": "DEMO_API_KEY",
            "target": ".env",
        });
        assert!(!record_secret_call_decision(
            &context,
            "write_file",
            &input,
            true
        ));
        assert!(!record_secret_call_decision(
            &context,
            "secret_apply",
            &serde_json::json!({ "secret_ref": "not-a-ref", "field": "X", "target": ".env" }),
            true
        ));
    }
}
