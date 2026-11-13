//! Workspace read, search and edit capability.
//!
//! `read`, `ls`, `find`, `grep`, `edit` and `write` are model-facing adapters
//! over the three implementations here. Keeping the capability in one module —
//! not one crate per capability — is deliberate: a responsibility boundary
//! does not need a compilation boundary (`docs/ARCHITECTURE.md` §5.3).

pub(crate) mod editor;
pub(crate) mod reader;
pub(crate) mod search;

pub(crate) use editor::{Commit, WorkspaceEditor};

/// Reject materializing the runtime's secret placeholder into a file. The
/// model sees `[REDACTED]` where a real value was hidden; copying that text
/// back would permanently replace the credential with the placeholder.
///
/// `allow_redacted_literal` is a model-controlled integrity opt-in, not a
/// security boundary. It exists so a user who explicitly asked for the
/// literal text is not blocked, but the harness cannot verify that intent:
/// repository or tool text can persuade the model to set it, and the write is
/// then allowed. The guarantee is therefore narrow and worth stating exactly —
/// the default refusal prevents the ordinary read → rewrite data loss, while
/// the escape remains an explicit, transcript-visible decision. It is not a
/// control that stops a determined or induced model from writing the literal.
pub(crate) fn ensure_materializable(
    content: &str,
    allow_redacted_literal: bool,
) -> Result<(), String> {
    if allow_redacted_literal || !content.contains(leveler_core::SECRET_PLACEHOLDER) {
        return Ok(());
    }
    Err(format!(
        "refusing to write {} into a file: it is the runtime's placeholder for a hidden \
         secret value, not the file's literal bytes. Read the file again and keep the real \
         value, or — only when the user explicitly asked for that literal text — set \
         allow_redacted_literal: true.",
        leveler_core::SECRET_PLACEHOLDER
    ))
}
pub(crate) use reader::{Clip, ReadError, ReadWindow, WorkspaceReader};
pub(crate) use search::{DirEntry, GrepQuery, SearchError, WorkspaceSearch};

pub(crate) fn cwd_approval_reason(
    context: &crate::ToolContext,
    cwd: Option<&str>,
) -> Option<String> {
    if context.policy.unrestricted_execution() {
        return None;
    }
    context
        .require_workspace()
        .ok()?
        .resolve_command_cwd(cwd.unwrap_or("."), &context.write_scope())
        .err()
        .map(|e| e.to_string())
}

/// Admission uses the same path boundary that execution would otherwise reject.
/// Missing workspace/input remains an ordinary tool error, not a consent prompt.
pub(crate) fn path_approval_reason(
    context: &crate::ToolContext,
    path: &str,
    write: bool,
) -> Option<String> {
    if context.policy.unrestricted_execution() {
        return None;
    }
    // Memory files are governed by their own policy, not by the workspace
    // write scope. Asking for workspace consent here would prompt for a path
    // the execution layer is about to handle itself.
    if crate::memory_access::owns_path(context, path) {
        return None;
    }
    let workspace = context.execution_workspace().ok()?;
    if write {
        if let Some(reason) = context.write_path_denied(path) {
            return Some(reason);
        }
        workspace
            .resolve_for_write(path, &context.write_scope())
            .err()
            .map(|e| e.to_string())
    } else {
        workspace
            .resolve_for_read(path)
            .err()
            .map(|e| e.to_string())
    }
}

/// Resolve one explicit file effect using the same host identity as consumers.
pub(crate) fn filesystem_grant_bindings(
    context: &crate::ToolContext,
    path: &str,
    capabilities: &[leveler_core::Capability],
) -> Result<Vec<leveler_core::GrantBinding>, String> {
    use leveler_core::{Capability, GrantBinding, ResourceIdentity};
    let workspace = context
        .require_workspace()
        .map_err(|error| error.to_string())?;
    let path = std::path::Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.root().join(path)
    };
    let resource = leveler_execution::resolve_filesystem_resource(&absolute)?;
    let sensitive = leveler_execution::workspace::is_credential_path(&absolute)
        || match &resource {
            ResourceIdentity::FilesystemPath { canonical_path, .. } => {
                leveler_execution::workspace::is_credential_path(std::path::Path::new(
                    canonical_path,
                ))
            }
            _ => false,
        };
    let mut bindings = Vec::new();
    for capability in capabilities {
        let binding = GrantBinding {
            capability: *capability,
            resource: resource.clone(),
        };
        if !bindings.contains(&binding) {
            bindings.push(binding);
        }
        if sensitive {
            let raw = if *capability == Capability::FilesystemRead {
                Capability::CredentialRawRead
            } else {
                Capability::CredentialRawWrite
            };
            let binding = GrantBinding {
                capability: raw,
                resource: resource.clone(),
            };
            if !bindings.contains(&binding) {
                bindings.push(binding);
            }
        }
    }
    Ok(bindings)
}
