//! Host-owned, process-local custody for credentials the user handed over.
//!
//! This is the security boundary between "the person typed a key and
//! confirmed capture" and "the model may ask the host to use it". It has three
//! invariants:
//!
//! 1. A value never appears in [`Debug`], a persisted row, or a tool result.
//!    Callers reach it only through [`SecretVault::with_value`], which borrows
//!    it for the duration of one closure.
//! 2. A reference is useless outside the session that captured it. A guessed,
//!    malformed, expired or revoked reference reports the same
//!    [`VaultError::Unavailable`] as an unknown one, so probing learns
//!    nothing.
//! 3. Values are cleared when revoked, when they expire, when their session is
//!    closed, or when the vault drops. Memory copies made by the OS or by a
//!    child process are outside this boundary; the product must not claim
//!    otherwise.
//!
//! There is no encryption here because there is no key: the value lives in
//! this process's memory for the life of the capture. Long-term storage is a
//! separate product decision that reuses the provider credential store.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

/// How long a captured secret stays usable when no shorter bound is chosen.
pub const DEFAULT_SECRET_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Most captures one session may hold at once.
pub const MAX_SECRETS_PER_SESSION: usize = 32;

/// Largest value the vault accepts. A person pasting more than this is
/// pasting a file, not a credential.
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

/// Errors that are safe to show a caller. None of them contains the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VaultError {
    /// Unknown, cross-session, expired or revoked — deliberately one answer.
    #[error("secret is not available; ask the user to provide it again")]
    Unavailable,
    /// The reference was revoked by the user or its session closed.
    #[error("secret was revoked; ask the user to provide it again")]
    Revoked,
    /// The capture's lifetime ended.
    #[error("secret expired; ask the user to provide it again")]
    Expired,
    /// A value that cannot be a credential (empty, whitespace, no session).
    #[error("value cannot be captured")]
    Invalid,
    /// The value exceeds [`MAX_SECRET_BYTES`].
    #[error("value is too large to capture")]
    TooLarge,
    /// The session already holds [`MAX_SECRETS_PER_SESSION`] captures.
    #[error("too many captured secrets in this session")]
    TooMany,
}

/// A non-secret handle to one captured value.
///
/// The identifier is 128 random bits rendered as `sec_` plus 32 lowercase hex
/// characters. It carries no information about the value and **is not an
/// authorization token by itself**: the vault still checks the session and the
/// host still checks the tool call.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SecretRef(String);

impl SecretRef {
    /// Mint an unpredictable reference. Collisions are resolved by the vault.
    fn generate() -> Self {
        let mut bytes = [0u8; 16];
        if getrandom::getrandom(&mut bytes).is_err() {
            // The OS entropy source is a hard requirement. Falling back to a
            // predictable id would silently destroy the guessing bound.
            panic!("operating system entropy source unavailable");
        }
        let mut value = String::with_capacity(4 + 32);
        value.push_str("sec_");
        for byte in bytes {
            use std::fmt::Write;
            let _ = write!(value, "{byte:02x}");
        }
        Self(value)
    }

    /// Parse one on the wire or from a model tool call.
    pub fn parse(value: &str) -> Result<Self, VaultError> {
        let Some(hex) = value.strip_prefix("sec_") else {
            return Err(VaultError::Unavailable);
        };
        if hex.len() != 32
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(VaultError::Unavailable);
        }
        Ok(Self(value.to_string()))
    }

    /// The identifier as it appears in a transcript or a tool argument.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("SecretRef")
            .field(&self.0.as_str())
            .finish()
    }
}

impl std::fmt::Display for SecretRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Lifecycle of one capture, as the host reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretState {
    /// Captured, not yet used for an authorized write.
    Available,
    /// At least one authorized operation used it. Still resolvable so a
    /// retried write is idempotent.
    Used,
    /// The user or the session withdrew it. Never resolvable again.
    Revoked,
    /// Its lifetime ended. Never resolvable again.
    Expired,
}

struct Entry {
    value: Zeroizing<String>,
    session: String,
    key_hint: Option<String>,
    created: Instant,
    state: SecretState,
    /// Targets an authorized operation has written it to. Names only; never
    /// the value.
    targets: Vec<String>,
}

/// Process-local captures. One instance per runtime host, shared by every
/// session it serves; the session is stored on each entry and checked on every
/// access.
pub struct SecretVault {
    entries: Mutex<HashMap<SecretRef, Entry>>,
    ttl: Duration,
}

impl SecretVault {
    /// A vault with [`DEFAULT_SECRET_TTL`].
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_SECRET_TTL)
    }

    /// A vault whose captures expire after `ttl`.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SecretRef, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Take custody of `value` for `session`.
    pub fn capture(
        &self,
        session: &str,
        value: String,
        key_hint: Option<String>,
    ) -> Result<SecretRef, VaultError> {
        if session.is_empty() || value.trim().is_empty() {
            return Err(VaultError::Invalid);
        }
        if value.len() > MAX_SECRET_BYTES {
            return Err(VaultError::TooLarge);
        }
        self.sweep();
        let mut entries = self.lock();
        let live = entries
            .values()
            .filter(|entry| {
                entry.session == session
                    && !matches!(entry.state, SecretState::Revoked | SecretState::Expired)
            })
            .count();
        if live >= MAX_SECRETS_PER_SESSION {
            return Err(VaultError::TooMany);
        }
        let reference = loop {
            let candidate = SecretRef::generate();
            if !entries.contains_key(&candidate) {
                break candidate;
            }
        };
        entries.insert(
            reference.clone(),
            Entry {
                value: Zeroizing::new(value),
                session: session.to_string(),
                key_hint,
                created: Instant::now(),
                state: SecretState::Available,
                targets: Vec::new(),
            },
        );
        Ok(reference)
    }

    /// Borrow the value for one authorized operation. The closure runs while
    /// the vault holds its lock, so the value cannot outlive the call by
    /// accident.
    pub fn with_value<T>(
        &self,
        session: &str,
        reference: &SecretRef,
        operation: impl FnOnce(&str) -> T,
    ) -> Result<T, VaultError> {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(reference) else {
            return Err(VaultError::Unavailable);
        };
        if entry.session != session {
            return Err(VaultError::Unavailable);
        }
        match entry.state {
            SecretState::Revoked => return Err(VaultError::Revoked),
            SecretState::Expired => return Err(VaultError::Expired),
            SecretState::Available | SecretState::Used => {}
        }
        if self.is_expired(entry) {
            entry.state = SecretState::Expired;
            entry.value.clear();
            return Err(VaultError::Expired);
        }
        Ok(operation(entry.value.as_str()))
    }

    /// Current lifecycle state. Cross-session access reports the same
    /// [`VaultError::Unavailable`] as an unknown reference.
    pub fn state(&self, session: &str, reference: &SecretRef) -> Result<SecretState, VaultError> {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(reference) else {
            return Err(VaultError::Unavailable);
        };
        if entry.session != session {
            return Err(VaultError::Unavailable);
        }
        if !matches!(entry.state, SecretState::Revoked | SecretState::Expired)
            && self.is_expired(entry)
        {
            entry.state = SecretState::Expired;
            entry.value.clear();
        }
        Ok(entry.state)
    }

    /// The field/header name the value was captured under, when known.
    pub fn key_hint(&self, session: &str, reference: &SecretRef) -> Option<String> {
        self.lock()
            .get(reference)
            .filter(|entry| entry.session == session)
            .and_then(|entry| entry.key_hint.clone())
    }

    /// Record that an authorized operation used the value for `target`.
    pub fn mark_used(&self, session: &str, reference: &SecretRef, target: &str) {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(reference) else {
            return;
        };
        if entry.session != session {
            return;
        }
        if matches!(entry.state, SecretState::Revoked | SecretState::Expired)
            || self.is_expired(entry)
        {
            return;
        }
        entry.state = SecretState::Used;
        if !entry.targets.iter().any(|known| known == target) {
            if entry.targets.len() >= 16 {
                entry.targets.remove(0);
            }
            entry.targets.push(target.to_string());
        }
    }

    /// Revoke one capture. Returns whether it existed in this session.
    pub fn revoke(&self, session: &str, reference: &SecretRef) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(reference) else {
            return false;
        };
        if entry.session != session {
            return false;
        }
        entry.state = SecretState::Revoked;
        entry.value.clear();
        true
    }

    /// Revoke every capture of `session`, returning how many were live.
    pub fn revoke_session(&self, session: &str) -> usize {
        let mut entries = self.lock();
        let mut revoked = 0usize;
        for entry in entries.values_mut() {
            if entry.session == session
                && matches!(entry.state, SecretState::Available | SecretState::Used)
            {
                entry.state = SecretState::Revoked;
                entry.value.clear();
                revoked += 1;
            }
        }
        revoked
    }

    /// Number of captures in `session` that are still available or used.
    pub fn available_count(&self, session: &str) -> usize {
        self.lock()
            .values()
            .filter(|entry| {
                entry.session == session
                    && matches!(entry.state, SecretState::Available | SecretState::Used)
                    && !self.is_expired(entry)
            })
            .count()
    }

    /// Drop every expired entry, clearing its value.
    pub fn sweep(&self) {
        let mut entries = self.lock();
        entries.retain(|_, entry| {
            let expired = self.is_expired(entry);
            if expired {
                entry.state = SecretState::Expired;
                entry.value.clear();
            }
            !expired && !matches!(entry.state, SecretState::Revoked | SecretState::Expired)
        });
    }

    /// Total entries currently held, including used and revoked-but-not-yet-
    /// swept rows. Diagnostics only.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the vault holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn is_expired(&self, entry: &Entry) -> bool {
        entry.created.elapsed() >= self.ttl
    }
}

impl Default for SecretVault {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SecretVault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecretVault")
            .field("entries", &self.len())
            .field("ttl", &self.ttl)
            .finish()
    }
}
