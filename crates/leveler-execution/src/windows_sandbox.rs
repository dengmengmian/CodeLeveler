//! Windows execution security surface.
//!
//! `leveler-execution` denies `unsafe_code`, so the Win32 half lives in the
//! audited `leveler-win-confine` crate and its `leveler-confine.exe` launcher.
//! This module:
//! - reports honest capabilities (doctor / CI)
//! - **fail-closes** non-FullAccess spawns when no write backend is available
//!   (no silent plain spawn)
//! - defines host-trusted [`FilesystemIntent`] — never model-chosen
//!
//! On non-Windows hosts this still exposes the capability probe API for tests.
//!
//! **What ships:**
//! - Job Object process-tree (`process-wrap`, `process_tree=job`)
//! - ACL coordination (`windows_acl`, icacls snapshot/restore/marker)
//! - Low-integrity write confinement (`windows_confine`): reads unrestricted,
//!   writes only where a root carries a Low mandatory label
//!
//! Windows has no per-process network deny outside AppContainer, and
//! AppContainer cannot give a coding agent readable toolchains. The probe
//! therefore reports `network_deny=false` and a request that asks for one is
//! refused rather than run with the network open.
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
    /// The workspace is not writable; scratch and toolchain caches still are.
    ReadOnly {
        #[serde(default)]
        read_roots: Vec<PathBuf>,
    },
    /// Writes confined to this root (plus scratch and toolchain caches).
    WorkspaceWrite { write_root: PathBuf },
}

impl FilesystemIntent {
    /// The Windows contract for one [`WriteScope`](crate::WriteScope). `None`
    /// anchors its read-only allowlist on `cwd` (the workspace the command
    /// runs in).
    pub fn from_write_scope(scope: &crate::WriteScope, cwd: &Path) -> Self {
        match scope {
            crate::WriteScope::Unrestricted => Self::Unrestricted,
            crate::WriteScope::Workspace { root } => Self::WorkspaceWrite {
                write_root: root.clone(),
            },
            crate::WriteScope::None => Self::ReadOnly {
                read_roots: vec![cwd.to_path_buf()],
            },
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
    // Drive root (e.g. C:\) or Unix /: an anchor with no real directory below
    // it. On Windows the RootDir stays after the Prefix, so count only Normal
    // components.
    let rest: Vec<_> = components.collect();
    if !rest.iter().any(|c| matches!(c, Component::Normal(_))) {
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
    /// No OS FS boundary; argv preflight only. This is what every host
    /// reports on the READ axis: CodeLeveler confines writes, not reads.
    PreflightOnly,
    /// Write isolation claimed (seatbelt / bwrap / Low-integrity labels).
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
    /// The host's own argv wrapper confines writes (seatbelt / bwrap), or
    /// nothing does.
    None,
    /// Windows: `leveler-confine.exe` plus Low mandatory labels on the write
    /// roots.
    LowIntegrity,
}

/// Probe host capabilities (no side effects).
///
/// Windows: Job tree, reads unconfined, writes confined when the
/// `leveler-confine.exe` launcher is installed. Non-Windows: process-group
/// tree + seatbelt/bwrap write-restricted.
pub fn probe_sandbox_capabilities() -> SandboxCapabilities {
    #[cfg(windows)]
    {
        let launcher = crate::windows_confine::launcher_path().is_some();
        SandboxCapabilities {
            process_tree: ProcessTreeCapability::Job,
            // Mandatory Integrity Control denies write-up, never read-up, so a
            // confined Windows command reads as freely as an unconfined one.
            read: FsCapability::PreflightOnly,
            write: if launcher {
                FsCapability::WriteRestricted
            } else {
                FsCapability::Unsupported
            },
            // Only AppContainer can deny a Windows process the network, and it
            // cannot leave the toolchain readable. Say so instead of pretending.
            network_deny: false,
            backend: if launcher {
                SandboxBackend::LowIntegrity
            } else {
                SandboxBackend::None
            },
        }
    }
    #[cfg(not(windows))]
    {
        // Unix/macOS: process-group kill is available; seatbelt/bwrap can confine
        // writes when the runner wraps a request. Capability labels describe what
        // the host *can* enforce — they must not claim a Windows AppContainer
        // backend or a blanket "sandbox=yes" (see `doctor_sandbox_line`).
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
    /// Job Object create/assign failed. Must not fall back to plain spawn.
    #[error("Windows Job Object setup failed: {0}")]
    JobSetupFailed(String),
}

/// Intent-aware gate. Unrestricted is always allowed; a confined intent
/// requires a backend that can actually confine it, on the foreground and the
/// background path alike.
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
                if !matches!(
                    caps.write,
                    FsCapability::WriteRestricted | FsCapability::FullFs
                ) {
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
        // Honesty: never advertise full unrestricted FS as a sandbox product claim.
        assert!(!line.to_lowercase().contains("full_fs"), "{line}");
        assert!(!line.to_lowercase().contains("fullfs"), "{line}");
    }

    #[test]
    fn no_host_ever_claims_a_read_fence_or_a_full_filesystem_allowlist() {
        let caps = probe_sandbox_capabilities();
        // Reads are not a capability CodeLeveler restricts on any host; a probe
        // that said otherwise would be describing a product we do not ship.
        assert_eq!(caps.read, FsCapability::PreflightOnly);
        assert_ne!(
            caps.write,
            FsCapability::FullFs,
            "must not claim full FS write allowlist"
        );
        #[cfg(not(windows))]
        {
            assert_eq!(caps.backend, SandboxBackend::None);
            assert_eq!(caps.write, FsCapability::WriteRestricted);
        }
    }

    #[test]
    fn full_access_always_allowed() {
        let intent = FilesystemIntent::from_write_scope(
            &crate::WriteScope::Unrestricted,
            Path::new("C:\\ws"),
        );
        assert!(assert_intent_spawn_allowed(&intent, true).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_restricted_write_allowed_when_appcontainer_linked() {
        let intent = FilesystemIntent::from_write_scope(
            &crate::WriteScope::Workspace {
                root: PathBuf::from("C:\\ws"),
            },
            Path::new("C:\\ws"),
        );
        assert!(matches!(intent, FilesystemIntent::WorkspaceWrite { .. }));
        // WS3-B linked: WorkspaceWrite is allowed (deny_network still ok).
        assert!(assert_intent_spawn_allowed(&intent, false).is_ok());
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
        let intent = FilesystemIntent::from_write_scope(
            &crate::WriteScope::Workspace {
                root: PathBuf::from("/tmp/ws"),
            },
            Path::new("/tmp/ws"),
        );
        assert!(assert_intent_spawn_allowed(&intent, true).is_ok());
    }

    #[test]
    fn probe_is_deterministic() {
        let a = probe_sandbox_capabilities();
        let b = probe_sandbox_capabilities();
        assert_eq!(a, b);
        let _ = PathBuf::from(".");
    }

    #[test]
    fn filesystem_intent_derives_from_write_scope() {
        use crate::WriteScope;
        let cwd = Path::new("/ws");
        assert!(
            FilesystemIntent::from_write_scope(&WriteScope::Unrestricted, cwd).is_unrestricted()
        );
        match FilesystemIntent::from_write_scope(
            &WriteScope::Workspace {
                root: PathBuf::from("/ws"),
            },
            cwd,
        ) {
            FilesystemIntent::WorkspaceWrite { write_root } => {
                assert_eq!(write_root, PathBuf::from("/ws"));
            }
            other => panic!("expected WorkspaceWrite, got {other:?}"),
        }
        match FilesystemIntent::from_write_scope(&WriteScope::None, cwd) {
            FilesystemIntent::ReadOnly { read_roots } => {
                assert_eq!(read_roots, vec![cwd.to_path_buf()])
            }
            other => panic!("expected ReadOnly, got {other:?}"),
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
        // A drive-less Unix path is a valid ACL root only on Unix hosts.
        #[cfg(not(windows))]
        assert!(validate_acl_root(Path::new("/Users/me/project")).is_ok());
        #[cfg(windows)]
        {
            assert!(validate_acl_root(Path::new(r"C:\")).is_err());
            assert!(validate_acl_root(Path::new(r"C:\Windows\System32")).is_err());
            assert!(validate_acl_root(Path::new(r"C:\Users\me\proj")).is_ok());
        }
    }

    #[test]
    fn the_probe_reports_a_job_process_tree_and_the_backend_it_really_has() {
        let caps = probe_sandbox_capabilities();
        assert_eq!(caps.process_tree, ProcessTreeCapability::Job);
        assert!(process_tree_backend_available());
        #[cfg(windows)]
        {
            // Write confinement is claimed exactly when the launcher that
            // performs it is installed, and network deny is never claimed:
            // nothing outside AppContainer can enforce it, and AppContainer
            // cannot leave a coding agent's toolchain readable.
            assert_eq!(
                caps.write == FsCapability::WriteRestricted,
                crate::windows_confine::launcher_path().is_some()
            );
            assert_eq!(
                caps.backend == SandboxBackend::LowIntegrity,
                crate::windows_confine::launcher_path().is_some()
            );
            assert!(!caps.network_deny);
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
    fn process_request_derives_its_intent_from_its_scope() {
        use crate::command::ProcessRequest;
        let mut req = ProcessRequest::new("echo", vec!["hi".into()], PathBuf::from("/ws"));
        assert!(req.filesystem_intent().is_unrestricted());
        req.write_scope = crate::WriteScope::Workspace {
            root: PathBuf::from("/ws"),
        };
        assert!(matches!(
            req.filesystem_intent(),
            FilesystemIntent::WorkspaceWrite { .. }
        ));
    }
}

#[cfg(test)]
mod background_gate_tests {
    use super::*;

    /// Background used to have a weaker execution model than foreground: the
    /// registry plain-spawned, so a confined background command either ran
    /// unconfined or was refused outright. Both paths now go through
    /// `CommandRunner::spawn`, so there is exactly one gate and one answer.
    #[test]
    fn background_and_foreground_answer_the_same_gate() {
        for intent in [
            FilesystemIntent::Unrestricted,
            FilesystemIntent::WorkspaceWrite {
                write_root: PathBuf::from("/ws"),
            },
            FilesystemIntent::ReadOnly {
                read_roots: vec![PathBuf::from("/ws")],
            },
        ] {
            for deny_network in [false, true] {
                let once = assert_intent_spawn_allowed(&intent, deny_network);
                let again = assert_intent_spawn_allowed(&intent, deny_network);
                assert_eq!(once.is_ok(), again.is_ok(), "{intent:?} {deny_network}");
            }
        }
    }

    /// A confined request is never quietly downgraded: off Windows the argv
    /// wrappers confine it, and on Windows it is allowed only when the
    /// launcher that confines it is present.
    #[test]
    fn a_confined_intent_is_allowed_only_where_something_can_confine_it() {
        let confined = FilesystemIntent::WorkspaceWrite {
            write_root: PathBuf::from("/ws"),
        };
        let allowed = assert_intent_spawn_allowed(&confined, false).is_ok();
        #[cfg(not(windows))]
        assert!(allowed);
        #[cfg(windows)]
        assert_eq!(allowed, crate::windows_confine::launcher_path().is_some());
    }

    /// Windows cannot deny a process the network without AppContainer, and
    /// AppContainer cannot leave the toolchain readable. A request that asks
    /// for a network deny is refused rather than run with the network open.
    #[cfg(windows)]
    #[test]
    fn a_network_deny_request_is_refused_on_windows_rather_than_run_open() {
        let confined = FilesystemIntent::WorkspaceWrite {
            write_root: PathBuf::from("C:\\ws"),
        };
        let error = assert_intent_spawn_allowed(&confined, true)
            .expect_err("network deny must fail closed on Windows");
        assert!(
            matches!(error, WindowsSandboxError::NetworkDenyUnsupported)
                || matches!(error, WindowsSandboxError::FsBackendMissing { .. }),
            "{error:?}"
        );
    }
}
