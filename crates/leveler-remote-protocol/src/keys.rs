//! Ed25519 key material for the two identities the design defines: a paired
//! device and a runtime.
//!
//! Backed by `ring`, which is already linked through rustls — the signing layer
//! adds no new crate to the supply chain.

use base64::Engine as _;

use crate::envelope::EnvelopeError;

/// A private signing key. Held only by the identity it names: a device keeps
/// its key in the platform keystore, a runtime in `~/.leveler/remote/runtime_key`.
/// A relay never holds one, which is what makes it unable to forge frames.
pub struct SigningKey {
    pair: ring::signature::Ed25519KeyPair,
}

impl std::fmt::Debug for SigningKey {
    /// Deliberately opaque: a key must not reach a log through a stray `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

impl SigningKey {
    /// Build from a 32-byte Ed25519 seed.
    pub fn from_seed(seed: &[u8; 32]) -> Result<Self, EnvelopeError> {
        let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
            .map_err(|_| EnvelopeError::InvalidKey)?;
        Ok(Self { pair })
    }

    pub(crate) fn sign_bytes(&self, message: &[u8]) -> Vec<u8> {
        self.pair.sign(message).as_ref().to_vec()
    }

    /// The public half, to publish in a QR payload or a pairing request.
    pub fn verifying_key(&self) -> VerifyingKey {
        use ring::signature::KeyPair as _;

        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(self.pair.public_key().as_ref());
        VerifyingKey { bytes }
    }
}

/// A public verification key: 32 raw Ed25519 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyingKey {
    bytes: [u8; 32],
}

impl VerifyingKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self { bytes }
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Decode the `base64url`-without-padding form the design uses for
    /// `device_pubkey` and `runtime_pubkey`.
    pub fn from_base64url(encoded: &str) -> Result<Self, EnvelopeError> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| EnvelopeError::InvalidKey)?;
        let bytes: [u8; 32] = raw.try_into().map_err(|_| EnvelopeError::InvalidKey)?;
        Ok(Self { bytes })
    }

    pub fn to_base64url(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.bytes)
    }

    /// The fingerprint a user compares between their phone and their terminal
    /// when accepting a pairing: the first 8 bytes of `SHA-256(pubkey_raw)` as
    /// 16 lowercase hex characters.
    ///
    /// Accepting a pairing is the moment trust is established, and the user is
    /// confirming *this key* rather than a device nickname a relay chose. Both
    /// ends must therefore compute the fingerprint identically; this is the one
    /// definition, so a CLI and an APP that disagree cannot both be right.
    pub fn fingerprint(&self) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, &self.bytes);
        crate::envelope::hex_lower(&digest.as_ref()[..8])
    }

    /// The fingerprint grouped for reading aloud: `abcd efgh ijkl mnop`.
    pub fn fingerprint_display(&self) -> String {
        let hex = self.fingerprint();
        hex.as_bytes()
            .chunks(4)
            .map(|chunk| std::str::from_utf8(chunk).expect("hex is ascii"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(crate) fn verify_bytes(&self, message: &[u8], signature: &[u8]) -> bool {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &self.bytes)
            .verify(message, signature)
            .is_ok()
    }
}
