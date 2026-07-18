//! Windows execution security surface (WS0+).
//!
//! `leveler-execution` forbids `unsafe_code`. Real Job/AppContainer backends
//! must live in an audited safe wrapper crate or out-of-process helper. This
//! module:
//! - reports honest capabilities (doctor / CI)
//! - **fail-closes** non-FullAccess spawns that need FS restriction when no
//!   backend is available (no silent plain spawn)
//! - defines host-trusted [`FilesystemIntent`] (WS2) — never model-chosen
//!
//! On non-Windows hosts this still exposes the capability probe API for tests.
//!
//! **Claimed vs shipped (2026-07):**
//! - WS0 fail-closed + capability probe
//! - WS1 Job Object process-tree (`process-wrap`, `process_tree=job`)
//! - WS2 ACL coordination (`windows_acl`, icacls snapshot/restore/marker)
//! - WS3 AppContainer RO + write-restricted WW (`rappct`, Windows only)
//!
//! Doctor never reports `sandbox=yes` or “full FS”.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Host-trusted filesystem intent for a process spawn (WS2).
///
/// Produced only by the host policy layer — never from model tool arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FilesystemIntent {
    /// Explicit FullAccess — plain spawn (+ future Job for tree kill).
    Unrestricted,
    /// AppContainer allowlist read (future backend).
    ReadOnly {
        #[serde(default)]
        read_roots: Vec<PathBuf>,
    },
    /// Low-integrity / write-restricted (future backend).
    WorkspaceWrite {
        write_root: PathBuf,
        #[serde(default)]
        extra_read_roots: Vec<PathBuf>,
    },
}

impl FilesystemIntent {
    /// Conservative mapping from legacy `write_root` + full_access flags.
    ///
    /// Missing write_root with full_access → Unrestricted.
    /// write_root present → WorkspaceWrite (restricted).
    pub fn from_legacy(write_root: Option<&Path>, full_access: bool) -> Self {
        if full_access || write_root.is_none() {
            return Self::Unrestricted;
        }
        Self::WorkspaceWrite {
            write_root: write_root.unwrap().to_path_buf(),
            extra_read_roots: Vec::new(),
        }
    }

    pub fn is_unrestricted(&self) -> bool {
        matches!(self, Self::Unrestricted)
    }
}

/// Reject system / drive roots that must never receive ACL mutation (WS2 seam).
pub fn validate_acl_root(root: &Path) -> Result<(), String> {
    if root.as_os_str().is_empty() {
        return Err("empty ACL root".into());
    }
    if !root.is_absolute() {
        return Err(format!("ACL root must be absolute: {}", root.display()));
    }
    let mut components = root.components();
    match components.next() {
        Some(Component::Prefix(_)) | Some(Component::RootDir) => {}
        _ => return Err(format!("ACL root not rooted: {}", root.display())),
    }
    // Drive root (e.g. C:\) or Unix /
    let rest: Vec<_> = components.collect();
    if rest.is_empty() {
        return Err(format!("refusing drive/root path: {}", root.display()));
    }
    // Known system paths (Windows-oriented; also blocks obvious Unix system dirs).
    let s = root.to_string_lossy().to_ascii_lowercase();
    for banned in [
        "\\windows\\",
        "/windows/",
        "\\system32",
        "/system32",
        "\\program files",
        "/usr/",
        "/bin",
        "/sbin",
        "/etc",
    ] {
        if s.contains(banned) {
            return Err(format!("refusing system path for ACL: {}", root.display()));
        }
    }
    Ok(())
}

/// Process tree isolation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessTreeCapability {
    /// Only the direct child is tracked/killed on cancel.
    DirectChildOnly,
    /// Job Object (or equivalent) kills the whole process tree.
    Job,
}

/// Filesystem isolation level (used for both read and write axes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsCapability {
    /// No OS FS boundary; argv preflight only.
    PreflightOnly,
    /// AppContainer package allowlist (read axis).
    AppContainerAllowlist,
    /// Write isolation claimed (AppContainer RW grants / Low-IL equivalent).
    WriteRestricted,
    /// Explicit write denied (ReadOnly intent path).
    Denied,
    /// Full FS allowlist (read+write) claimed — dual-backend never sets this.
    FullFs,
    /// Platform / build does not support FS sandbox.
    Unsupported,
}

/// Aggregate sandbox capabilities for doctor / policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub process_tree: ProcessTreeCapability,
    pub read: FsCapability,
    pub write: FsCapability,
    /// Whether network deny can be enforced at OS level.
    pub network_deny: bool,
    pub backend: SandboxBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    None,
    /// Future: AppContainer read-only path.
    AppContainer,
    /// Future: Low-integrity write-restricted path.
    LowIntegrity,
}

/// Probe host capabilities (no side effects).
///
/// Windows: Job tree + AppContainer FS axes when `rappct` path is linked.
/// Non-Windows: process-group tree + seatbelt/bwrap write-restricted.
pub fn probe_sandbox_capabilities() -> SandboxCapabilities {
    #[cfg(windows)]
    {
        let ac = crate::windows_appcontainer::appcontainer_backend_linked();
        SandboxCapabilities {
            process_tree: ProcessTreeCapability::Job,
            read: if ac {
                FsCapability::AppContainerAllowlist
            } else {
                FsCapability::PreflightOnly
            },
            write: if ac {
                FsCapability::WriteRestricted
            } else {
                FsCapability::Unsupported
            },
            // AppContainer can omit InternetClient when deny_network=true.
            network_deny: ac,
            backend: if ac {
                SandboxBackend::AppContainer
            } else {
                SandboxBackend::None
            },
        }
    }
    #[cfg(not(windows))]
    {
        SandboxCapabilities {
            process_tree: ProcessTreeCapability::Job,
            read: FsCapability::PreflightOnly,
            write: FsCapability::WriteRestricted,
            network_deny: true,
            backend: SandboxBackend::None,
        }
    }
}

/// Whether process-tree (Job / process-group) control is available on this build.
pub fn process_tree_backend_available() -> bool {
    matches!(
        probe_sandbox_capabilities().process_tree,
        ProcessTreeCapability::Job
    )
}

/// Typed refusal when Windows would otherwise plain-spawn under a restricted request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WindowsSandboxError {
    #[error(
        "Windows FS sandbox is not available (capability write=unsupported); \
         refuse non-FullAccess spawn with write_root={write_root:?}. \
         Use FullAccess explicitly, or ensure Job/AppContainer backend is available"
    )]
    FsBackendMissing { write_root: String },
    #[error("Windows network deny is not available for this backend")]
    NetworkDenyUnsupported,
    /// Job Object create/assign failed (WS1). Must not fall back to plain spawn.
    #[error("Windows Job Object setup failed: {0}")]
    JobSetupFailed(String),
}

/// Whether a restricted process request may proceed on this host.
///
/// **WS0 hard rule:** on Windows, any request with `write_root=Some` must not
/// plain-spawn. Until a real backend ships, return [`WindowsSandboxError`].
pub fn assert_windows_spawn_allowed(
    write_root: Option<&Path>,
    deny_network: bool,
    full_access: bool,
) -> Result<(), WindowsSandboxError> {
    let intent = FilesystemIntent::from_legacy(write_root, full_access);
    assert_intent_spawn_allowed(&intent, deny_network)
}

/// Intent-aware gate (WS2). Unrestricted always allowed; restricted intents
/// require matching capability backends.
pub fn assert_intent_spawn_allowed(
    intent: &FilesystemIntent,
    deny_network: bool,
) -> Result<(), WindowsSandboxError> {
    #[cfg(not(windows))]
    {
        let _ = (intent, deny_network);
        Ok(())
    }
    #[cfg(windows)]
    {
        if intent.is_unrestricted() {
            return Ok(());
        }
        let caps = probe_sandbox_capabilities();
        match intent {
            FilesystemIntent::Unrestricted => Ok(()),
            FilesystemIntent::ReadOnly { .. } => {
                if !matches!(caps.backend, SandboxBackend::AppContainer)
                    || !matches!(caps.read, FsCapability::AppContainerAllowlist)
                {
                    return Err(WindowsSandboxError::FsBackendMissing {
                        write_root: "(read-only intent)".into(),
                    });
                }
                if deny_network && !caps.network_deny {
                    return Err(WindowsSandboxError::NetworkDenyUnsupported);
                }
                Ok(())
            }
            FilesystemIntent::WorkspaceWrite { write_root, .. } => {
                if !matches!(
                    caps.write,
                    FsCapability::WriteRestricted | FsCapability::FullFs
                ) {
                    return Err(WindowsSandboxError::FsBackendMissing {
                        write_root: write_root.display().to_string(),
                    });
                }
                if deny_network && !caps.network_deny {
                    return Err(WindowsSandboxError::NetworkDenyUnsupported);
                }
                Ok(())
            }
        }
    }
}

/// Doctor one-liner (never claims sandbox=yes when unsupported).
pub fn doctor_sandbox_line() -> String {
    let c = probe_sandbox_capabilities();
    format!(
        "process_tree={:?} read={:?} write={:?} network_deny={} backend={:?}",
        c.process_tree, c.read, c.write, c.network_deny, c.backend
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn doctor_line_never_says_sandbox_yes() {
        let line = doctor_sandbox_line();
        assert!(!line.to_lowercase().contains("sandbox=yes"), "{line}");
        assert!(!line.to_lowercase().contains("sandbox=yes"));
    }

    #[test]
    fn full_access_always_allowed() {
        assert!(assert_windows_spawn_allowed(Some(Path::new("C:\\ws")), true, true).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_restricted_write_allowed_when_appcontainer_linked() {
        let intent = FilesystemIntent::from_legacy(Some(Path::new("C:\\ws")), false);
        assert!(matches!(intent, FilesystemIntent::WorkspaceWrite { .. }));
        // WS3-B linked: WorkspaceWrite is allowed (deny_network still ok).
        assert!(assert_intent_spawn_allowed(&intent, false).is_ok());
        assert!(assert_windows_spawn_allowed(Some(Path::new("C:\\ws")), false, false).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_readonly_intent_allowed_when_appcontainer_linked() {
        let intent = FilesystemIntent::ReadOnly {
            read_roots: vec![PathBuf::from(r"C:\ws")],
        };
        assert!(assert_intent_spawn_allowed(&intent, true).is_ok());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_allows_restricted_for_seatbelt_path() {
        assert!(assert_windows_spawn_allowed(Some(Path::new("/tmp/ws")), true, false).is_ok());
    }

    #[test]
    fn probe_is_deterministic() {
        let a = probe_sandbox_capabilities();
        let b = probe_sandbox_capabilities();
        assert_eq!(a, b);
        let _ = PathBuf::from(".");
    }

    #[test]
    fn filesystem_intent_legacy_defaults_conservative() {
        assert!(FilesystemIntent::from_legacy(None, false).is_unrestricted());
        assert!(FilesystemIntent::from_legacy(Some(Path::new("/ws")), true).is_unrestricted());
        match FilesystemIntent::from_legacy(Some(Path::new("/ws")), false) {
            FilesystemIntent::WorkspaceWrite { write_root, .. } => {
                assert_eq!(write_root, PathBuf::from("/ws"));
            }
            other => panic!("expected WorkspaceWrite, got {other:?}"),
        }
    }

    #[test]
    fn filesystem_intent_serde_round_trip() {
        let intent = FilesystemIntent::ReadOnly {
            read_roots: vec![PathBuf::from("/repo")],
        };
        let v = serde_json::to_value(&intent).unwrap();
        let back: FilesystemIntent = serde_json::from_value(v).unwrap();
        assert_eq!(back, intent);
    }

    #[test]
    fn acl_root_validator_rejects_drive_and_system() {
        assert!(validate_acl_root(Path::new("/")).is_err());
        assert!(validate_acl_root(Path::new("relative/path")).is_err());
        assert!(validate_acl_root(Path::new("/usr/bin")).is_err());
        assert!(validate_acl_root(Path::new("/Users/me/project")).is_ok());
        #[cfg(windows)]
        {
            assert!(validate_acl_root(Path::new(r"C:\")).is_err());
            assert!(validate_acl_root(Path::new(r"C:\Windows\System32")).is_err());
            assert!(validate_acl_root(Path::new(r"C:\Users\me\proj")).is_ok());
        }
    }

    #[test]
    fn windows_probe_claims_job_tree_and_appcontainer_when_linked() {
        let caps = probe_sandbox_capabilities();
        assert_eq!(caps.process_tree, ProcessTreeCapability::Job);
        assert!(process_tree_backend_available());
        #[cfg(windows)]
        {
            assert_eq!(caps.read, FsCapability::AppContainerAllowlist);
            assert_eq!(caps.write, FsCapability::WriteRestricted);
            assert_eq!(caps.backend, SandboxBackend::AppContainer);
            assert!(caps.network_deny);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(caps.write, FsCapability::WriteRestricted);
            assert_eq!(caps.backend, SandboxBackend::None);
        }
        let line = doctor_sandbox_line();
        assert!(
            line.to_lowercase().contains("job") || line.contains("Job"),
            "doctor must report job process tree: {line}"
        );
        assert!(!line.to_lowercase().contains("sandbox=yes"), "{line}");
        assert!(!line.to_lowercase().contains("full_fs") && !line.contains("FullFs"));
    }

    #[test]
    fn process_request_carries_filesystem_intent_field() {
        use crate::command::ProcessRequest;
        let mut req = ProcessRequest::new("echo", vec!["hi".into()], PathBuf::from("."));
        assert!(req.filesystem_intent.is_none());
        req.filesystem_intent = Some(FilesystemIntent::WorkspaceWrite {
            write_root: PathBuf::from("/ws"),
            extra_read_roots: Vec::new(),
        });
        assert!(matches!(
            req.filesystem_intent,
            Some(FilesystemIntent::WorkspaceWrite { .. })
        ));
        // Legacy mapping still used when intent is None.
        let legacy = FilesystemIntent::from_legacy(Some(Path::new("/ws")), false);
        assert!(matches!(legacy, FilesystemIntent::WorkspaceWrite { .. }));
    }
}
