//! WS2 ACL / integrity coordination for Windows FS backends.
//!
//! Operations shell out to absolute `%SystemRoot%\System32\icacls.exe` (no
//! PATH, no shell). Used before AppContainer grants or Low-IL labels so roots
//! can be snapshotted and restored. All mutations are marker-before-write and
//! fail closed on restore/lock errors.
//!
//! On non-Windows hosts this module still exposes validators and a
//! [`AclCoordinator`] that refuses mutation with a clear error (tests can
//! exercise the pure path).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::windows_sandbox::validate_acl_root;

/// Global per-root serialization (process-local; named mutex is Windows-only).
fn root_locks() -> &'static Mutex<HashMap<String, ()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, ()>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Snapshot of one root's ACL state (opaque icacls dump).
#[derive(Debug, Clone)]
pub struct AclSnapshot {
    pub root: PathBuf,
    pub dump: String,
}

/// Marker file written **before** any ACL mutation.
#[derive(Debug, Clone)]
pub struct ResidueMarker {
    pub path: PathBuf,
    pub root: PathBuf,
    pub created_unix: u64,
}

/// Coordinates ACL mutations for a set of roots.
#[derive(Debug, Default)]
pub struct AclCoordinator {
    snapshots: Vec<AclSnapshot>,
    markers: Vec<ResidueMarker>,
    locked_keys: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AclError {
    #[error("invalid ACL root: {0}")]
    InvalidRoot(String),
    #[error("ACL root lock busy: {0}")]
    LockBusy(String),
    #[error("icacls failed: {0}")]
    Icacls(String),
    #[error("marker write failed: {0}")]
    Marker(String),
    #[error("restore failed for {root}: {detail}")]
    Restore { root: String, detail: String },
    #[error("ACL mutation unsupported on this platform: {0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl AclCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate, lock, snapshot, and drop a residue marker for `root`.
    pub fn prepare_root(&mut self, root: &Path) -> Result<(), AclError> {
        validate_acl_root(root).map_err(AclError::InvalidRoot)?;
        let key = normalize_root_key(root);
        {
            let mut map = root_locks()
                .lock()
                .map_err(|_| AclError::LockBusy(key.clone()))?;
            if map.contains_key(&key) {
                return Err(AclError::LockBusy(key));
            }
            map.insert(key.clone(), ());
        }
        self.locked_keys.push(key);

        let marker = write_marker(root)?;
        self.markers.push(marker);

        let snap = snapshot_root(root)?;
        self.snapshots.push(snap);
        Ok(())
    }

    /// Restore all snapshotted roots and clear markers. Loud on failure.
    pub fn restore_all(&mut self) -> Result<(), AclError> {
        let mut first_err: Option<AclError> = None;
        for snap in self.snapshots.drain(..).rev() {
            if let Err(e) = restore_root(&snap)
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }
        for marker in self.markers.drain(..) {
            let _ = fs::remove_file(&marker.path);
        }
        self.release_locks();
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn release_locks(&mut self) {
        if let Ok(mut map) = root_locks().lock() {
            for key in self.locked_keys.drain(..) {
                map.remove(&key);
            }
        } else {
            self.locked_keys.clear();
        }
    }
}

impl Drop for AclCoordinator {
    fn drop(&mut self) {
        let _ = self.restore_all();
    }
}

fn normalize_root_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .to_ascii_lowercase()
}

fn marker_path(root: &Path) -> PathBuf {
    root.join(".leveler-acl-marker")
}

fn write_marker(root: &Path) -> Result<ResidueMarker, AclError> {
    let path = marker_path(root);
    let created_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = format!(
        "leveler-acl-marker\nroot={}\ncreated_unix={created_unix}\n",
        root.display()
    );
    fs::write(&path, body).map_err(|e| AclError::Marker(e.to_string()))?;
    Ok(ResidueMarker {
        path,
        root: root.to_path_buf(),
        created_unix,
    })
}

/// Absolute path to `icacls.exe` (never PATH).
pub fn icacls_path() -> PathBuf {
    #[cfg(windows)]
    {
        let system_root = leveler_core::environment()
            .var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        system_root.join("System32").join("icacls.exe")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/usr/bin/false")
    }
}

fn snapshot_root(root: &Path) -> Result<AclSnapshot, AclError> {
    #[cfg(not(windows))]
    {
        // Pure coordinator tests on non-Windows: record a stub snapshot so
        // restore is a no-op success after marker lifecycle is exercised.
        Ok(AclSnapshot {
            root: root.to_path_buf(),
            dump: "non-windows-stub".into(),
        })
    }
    #[cfg(windows)]
    {
        let out = Command::new(icacls_path())
            .arg(root)
            .arg("/save")
            .arg(root.join(".leveler-acl-snapshot.txt"))
            .arg("/T")
            .arg("/C")
            .output()
            .map_err(|e| AclError::Icacls(e.to_string()))?;
        if !out.status.success() {
            // Fallback: capture textual ACL listing when /save fails (permissions).
            let list = Command::new(icacls_path())
                .arg(root)
                .output()
                .map_err(|e| AclError::Icacls(e.to_string()))?;
            if !list.status.success() {
                return Err(AclError::Icacls(format!(
                    "icacls snapshot failed: {}",
                    String::from_utf8_lossy(&list.stderr)
                )));
            }
            return Ok(AclSnapshot {
                root: root.to_path_buf(),
                dump: String::from_utf8_lossy(&list.stdout).into_owned(),
            });
        }
        let dump_path = root.join(".leveler-acl-snapshot.txt");
        let dump = fs::read_to_string(&dump_path).unwrap_or_default();
        Ok(AclSnapshot {
            root: root.to_path_buf(),
            dump,
        })
    }
}

fn restore_root(snap: &AclSnapshot) -> Result<(), AclError> {
    #[cfg(not(windows))]
    {
        let _ = snap;
        Ok(())
    }
    #[cfg(windows)]
    {
        let dump_path = snap.root.join(".leveler-acl-snapshot.txt");
        if dump_path.exists() {
            let out = Command::new(icacls_path())
                .arg(snap.root.parent().unwrap_or(Path::new(".")))
                .arg("/restore")
                .arg(&dump_path)
                .arg("/T")
                .arg("/C")
                .output()
                .map_err(|e| AclError::Restore {
                    root: snap.root.display().to_string(),
                    detail: e.to_string(),
                })?;
            let _ = fs::remove_file(&dump_path);
            if !out.status.success() {
                return Err(AclError::Restore {
                    root: snap.root.display().to_string(),
                    detail: String::from_utf8_lossy(&out.stderr).into_owned(),
                });
            }
            return Ok(());
        }
        // Text dump only: cannot auto-restore; surface loud error.
        if snap.dump != "non-windows-stub" && !snap.dump.is_empty() {
            return Err(AclError::Restore {
                root: snap.root.display().to_string(),
                detail: "no machine-readable ACL snapshot to restore".into(),
            });
        }
        Ok(())
    }
}

/// Sweep residue markers under `root` (startup cleanup).
pub fn sweep_residue_markers(root: &Path) -> Result<usize, AclError> {
    validate_acl_root(root).map_err(AclError::InvalidRoot)?;
    let marker = marker_path(root);
    if marker.exists() {
        // Best-effort: remove marker; real ACL restore requires snapshot files.
        fs::remove_file(&marker)?;
        let snap = root.join(".leveler-acl-snapshot.txt");
        if snap.exists() {
            let snapshot = AclSnapshot {
                root: root.to_path_buf(),
                dump: fs::read_to_string(&snap).unwrap_or_default(),
            };
            restore_root(&snapshot)?;
        }
        return Ok(1);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique, auto-cleaned directory (avoids parallel races on shared temp paths).
    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn prepare_writes_marker_before_any_restore() {
        let dir = temp_root();
        let root = dir.path();
        let mut c = AclCoordinator::new();
        c.prepare_root(root).unwrap();
        assert!(
            marker_path(root).exists(),
            "marker must exist after prepare"
        );
        c.restore_all().unwrap();
        assert!(!marker_path(root).exists(), "marker cleared on restore");
    }

    #[test]
    fn rejects_system_roots() {
        let mut c = AclCoordinator::new();
        assert!(c.prepare_root(Path::new("/")).is_err());
        assert!(c.prepare_root(Path::new("relative")).is_err());
    }

    #[test]
    fn double_prepare_same_root_is_lock_busy() {
        let dir = temp_root();
        let root = dir.path();
        let mut a = AclCoordinator::new();
        a.prepare_root(root).unwrap();
        let mut b = AclCoordinator::new();
        let err = b.prepare_root(root).unwrap_err();
        assert!(matches!(err, AclError::LockBusy(_)), "{err:?}");
        a.restore_all().unwrap();
    }

    #[test]
    fn sweep_clears_orphan_marker() {
        let dir = temp_root();
        let root = dir.path();
        write_marker(root).unwrap();
        assert!(marker_path(root).exists());
        let n = sweep_residue_markers(root).unwrap();
        assert_eq!(n, 1);
        assert!(!marker_path(root).exists());
    }

    #[test]
    fn restore_releases_lock_so_prepare_can_retry() {
        let dir = temp_root();
        let root = dir.path();
        let mut a = AclCoordinator::new();
        a.prepare_root(root).unwrap();
        a.restore_all().unwrap();
        let mut b = AclCoordinator::new();
        b.prepare_root(root).unwrap();
        assert!(marker_path(root).exists());
        b.restore_all().unwrap();
    }

    #[test]
    fn drop_restores_and_clears_marker() {
        let dir = temp_root();
        let root = dir.path().to_path_buf();
        {
            let mut c = AclCoordinator::new();
            c.prepare_root(&root).unwrap();
            assert!(marker_path(&root).exists());
        }
        assert!(
            !marker_path(&root).exists(),
            "Drop must clear marker via restore_all"
        );
        let mut again = AclCoordinator::new();
        again.prepare_root(&root).unwrap();
        again.restore_all().unwrap();
    }

    #[test]
    fn icacls_path_is_absolute_never_bare_name() {
        let p = icacls_path();
        assert!(p.is_absolute() || cfg!(not(windows)), "{p:?}");
        #[cfg(windows)]
        {
            assert!(
                p.ends_with("icacls.exe") || p.ends_with("icacls.EXE"),
                "{p:?}"
            );
            let s = p.to_string_lossy();
            assert!(
                !s.eq_ignore_ascii_case("icacls"),
                "must not be bare PATH name"
            );
        }
    }

    #[test]
    fn prepare_then_restore_is_idempotent_on_second_restore() {
        let dir = temp_root();
        let root = dir.path();
        let mut c = AclCoordinator::new();
        c.prepare_root(root).unwrap();
        c.restore_all().unwrap();
        c.restore_all().unwrap();
        assert!(!marker_path(root).exists());
    }
}
