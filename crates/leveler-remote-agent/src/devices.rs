//! The agent's own record of which devices it trusts.
//!
//! This file is the **only** source of device public keys. Nothing arriving
//! over the tunnel updates it — not `open_stream`, not a re-sent
//! `pairing_pending`, not anything else a relay can author. That restriction is
//! the whole point of trust-on-first-use here: the user confirmed a fingerprint
//! on their own terminal, and a relay that could later substitute a key would
//! make that confirmation meaningless while leaving the signature check looking
//! like it still worked.
//!
//! Because the file is the authority, every lookup reads it. The alternative —
//! caching at startup — makes two ordinary things quietly not work: a device
//! paired while the agent is running stays untrusted, and a revocation does not
//! take effect until someone restarts the process, which is exactly the window
//! the threat model calls "revoked but still delivering". Upstream frames are
//! user-initiated and rare, so a small read costs nothing next to the signature
//! verification that follows it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use leveler_remote_protocol::pairing::{DeviceStore, PairedDevice, PairingScope};
use leveler_remote_protocol::{EnvelopeError, VerifyingKey};

/// Why a device could not be trusted for this frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    #[error("device {device_id} is not paired with this runtime")]
    NotPaired { device_id: String },
    #[error("device {device_id} was revoked")]
    Revoked { device_id: String },
    #[error("stored key for device {device_id} is unusable")]
    UnusableKey { device_id: String },
    #[error("the relay offered a different public key for device {device_id}")]
    PubkeyMismatch { device_id: String },
}

impl TrustError {
    /// The wire code from the design's catalogue.
    pub fn code(&self) -> &'static str {
        match self {
            TrustError::NotPaired { .. } => "unauthorized",
            TrustError::Revoked { .. } => "device_revoked",
            TrustError::UnusableKey { .. } => "invalid_frame",
            TrustError::PubkeyMismatch { .. } => "pubkey_mismatch",
        }
    }
}

/// Reads and writes `~/.leveler/remote/devices.json`.
#[derive(Debug)]
pub struct TrustedDevices {
    path: PathBuf,
    /// Last known-good contents, used only when the file cannot be read — a
    /// truncated write must not drop trust in every device mid-flight.
    cached: Mutex<DeviceStore>,
}

impl TrustedDevices {
    /// Load the store, treating a missing file as "nothing is paired yet".
    pub fn load(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let store = read_store(&path)?;
        Ok(Self {
            path,
            cached: Mutex::new(store),
        })
    }

    /// The devices on disk right now.
    pub fn devices(&self) -> Vec<PairedDevice> {
        self.current().devices
    }

    /// The file's contents, falling back to the last good copy if it cannot be
    /// read or parsed.
    fn current(&self) -> DeviceStore {
        match read_store(&self.path) {
            Ok(store) => {
                *self.cached.lock().unwrap() = store.clone();
                store
            }
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "could not read the device store; using the last good copy"
                );
                self.cached.lock().unwrap().clone()
            }
        }
    }

    /// Record a device the user accepted.
    ///
    /// Writing here is what establishes trust, so it happens only on an explicit
    /// local accept — never from a tunnel frame.
    pub fn accept(
        &mut self,
        device_id: &str,
        key: &VerifyingKey,
        name: &str,
        scope: PairingScope,
        paired_at: &str,
    ) -> std::io::Result<()> {
        // Read-modify-write against what is on disk, not against whatever this
        // handle saw when it was created: another process may have paired a
        // device since.
        let mut store = self.current();
        store.devices.retain(|device| device.device_id != device_id);
        store.devices.push(PairedDevice {
            device_id: device_id.to_string(),
            device_pubkey_b64: key.to_base64url(),
            fingerprint: key.fingerprint(),
            name: name.to_string(),
            scope,
            paired_at: paired_at.to_string(),
            revoked_at: None,
        });
        self.persist(&store)
    }

    /// Mark a device revoked, keeping the row so an operator can still see what
    /// was paired and when it was withdrawn.
    pub fn revoke(&mut self, device_id: &str, revoked_at: &str) -> std::io::Result<bool> {
        let mut store = self.current();
        let Some(device) = store
            .devices
            .iter_mut()
            .find(|device| device.device_id == device_id)
        else {
            return Ok(false);
        };
        device.revoked_at = Some(revoked_at.to_string());
        self.persist(&store)?;
        Ok(true)
    }

    /// The key to verify `device_id` against.
    ///
    /// `relay_claimed_pubkey` is whatever the relay attached to the frame, if
    /// anything. It is never used as the key — only compared, so a substitution
    /// attempt is reported rather than silently tolerated.
    pub fn key_for(
        &self,
        device_id: &str,
        relay_claimed_pubkey: Option<&str>,
    ) -> Result<(VerifyingKey, PairingScope), TrustError> {
        // From disk, every time: pairing and revocation happen in another
        // process, and both must take effect on the very next frame.
        let store = self.current();
        let device = store
            .devices
            .iter()
            .find(|device| device.device_id == device_id)
            .ok_or_else(|| TrustError::NotPaired {
                device_id: device_id.to_string(),
            })?;

        if !device.is_active() {
            return Err(TrustError::Revoked {
                device_id: device_id.to_string(),
            });
        }

        if let Some(claimed) = relay_claimed_pubkey
            && claimed != device.device_pubkey_b64
        {
            return Err(TrustError::PubkeyMismatch {
                device_id: device_id.to_string(),
            });
        }

        let key = VerifyingKey::from_base64url(&device.device_pubkey_b64).map_err(|error| {
            debug_assert!(matches!(error, EnvelopeError::InvalidKey));
            TrustError::UnusableKey {
                device_id: device_id.to_string(),
            }
        })?;
        Ok((key, device.scope))
    }

    /// Write the store, replacing the file atomically so a crash mid-write
    /// cannot leave a truncated list of trusted keys behind.
    fn persist(&self, store: &DeviceStore) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
            restrict(parent, 0o700)?;
        }
        let temporary = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(store).map_err(std::io::Error::other)?;
        std::fs::write(&temporary, &bytes)?;
        restrict(&temporary, 0o600)?;
        std::fs::rename(&temporary, &self.path)?;
        *self.cached.lock().unwrap() = store.clone();
        Ok(())
    }
}

/// Read the store, treating a missing file as "nothing is paired yet".
fn read_store(path: &Path) -> std::io::Result<DeviceStore> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DeviceStore::default()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}
