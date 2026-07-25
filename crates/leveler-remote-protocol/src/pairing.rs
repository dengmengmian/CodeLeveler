//! Device pairing: how a phone becomes trusted by a runtime, exactly once.
//!
//! The security-relevant property is that the *host* decides. A relay relays
//! the request and can lie about everything in it, so the user confirms a
//! fingerprint of the device's public key on their own terminal, and the agent
//! records that key locally. From then on the agent verifies against its own
//! store — never against a key a relay offers alongside a frame, which is the
//! substitution this flow exists to prevent.

use serde::{Deserialize, Serialize};

/// What a paired device is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PairingScope {
    /// Full interactive control, still narrowed by the remote allowlist.
    #[default]
    Interactive,
    /// Read-only: events and snapshots arrive, every mutating command is
    /// refused. Not a lower-trust data plane — the same signed session stream,
    /// with delivery denied.
    Observe,
}

/// Where a pairing stands. Only the host can move it to `Active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    /// Secret issued, no device has claimed it yet.
    PendingApp,
    /// A device claimed it; waiting for the human at the host to accept.
    PendingConfirm,
    Active,
    Rejected,
    Expired,
    Revoked,
}

/// `POST /v1/pair/complete` — the device claims a pairing secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairCompleteRequest {
    pub device_id: String,
    /// Raw Ed25519 public key, base64url unpadded. The value the user is
    /// really confirming when they accept.
    pub device_pubkey: String,
    pub device_name: String,
    pub platform: String,
    /// The ≥128-bit secret from the QR payload.
    pub pairing_secret: String,
    #[serde(default)]
    pub scope: PairingScope,
}

/// What the QR encodes. Carries `runtime_pubkey` so the APP anchors the
/// runtime's identity out-of-band, rather than trusting the relay's claim
/// about which key belongs to which host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingQrPayload {
    pub runtime_id: String,
    pub runtime_pubkey: String,
    pub relay_url: String,
    pub pairing_secret: String,
}

/// Relay → agent: a device is waiting for this host to accept it.
///
/// `device_pubkey` is the field the user is asked to confirm by fingerprint. A
/// relay that substitutes it is asking the user to trust a key the relay
/// controls, which is why acceptance persists this exact value and later frames
/// are checked against the stored copy rather than against anything re-sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingPending {
    pub pairing_id: String,
    pub device_id: String,
    pub device_pubkey: String,
    pub device_name: String,
    pub platform: String,
    pub scope: PairingScope,
}

/// The host's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairDecision {
    Accept,
    Reject,
}

/// One accepted device, as persisted in `~/.leveler/remote/devices.json`.
///
/// This file is the agent's only source of device keys. Nothing on the wire
/// updates it; rotating a key requires pairing again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub device_id: String,
    pub device_pubkey_b64: String,
    /// Stored so the value the user confirmed can be shown again later.
    pub fingerprint: String,
    pub name: String,
    pub scope: PairingScope,
    pub paired_at: String,
    #[serde(default)]
    pub revoked_at: Option<String>,
}

impl PairedDevice {
    /// Whether this device may still be trusted.
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// The on-disk shape of `devices.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStore {
    #[serde(default)]
    pub devices: Vec<PairedDevice>,
}

impl DeviceStore {
    /// The key to verify `device_id` against, if it is paired and not revoked.
    ///
    /// Returns `None` for a revoked device, so revocation takes effect on the
    /// next frame rather than at the next reconnection.
    pub fn active_key_for(&self, device_id: &str) -> Option<&PairedDevice> {
        self.devices
            .iter()
            .find(|device| device.device_id == device_id && device.is_active())
    }
}
