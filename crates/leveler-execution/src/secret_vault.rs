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

/// One exact use the person approved or refused for a captured reference.
///
/// The authorization unit is `(reference, target, field)`, never the reference
/// alone: approving one use must not let the model choose another target or
/// field for the same value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SecretExactUse {
    /// Workspace-relative dotenv target, normalized by the text layer.
    pub target: String,
    /// Dotenv field name.
    pub field: String,
}

/// The uses the user's own messages named for one captured value.
///
/// This is authorization data, not a detection result. An empty target set
/// means the person never named a dotenv file for this capture, so the host
/// cannot confine the write to one the model chose. A non-empty set is a hard
/// bound — the broker may write the value only to those targets and fields.
/// `denied_targets` and `denied_fields` are hard prohibitions in every mode.
/// `ambiguous` records that a target or field was named but not attributable
/// to a reference; an unbound choice must then be confirmed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretUseScope {
    /// Workspace-relative dotenv targets, normalized by
    /// `leveler_core::normalize_dotenv_target`.
    pub targets: Vec<String>,
    /// Dotenv field names explicitly assigned in the user's text.
    pub fields: Vec<String>,
    /// Targets the user explicitly prohibited for this capture.
    pub denied_targets: Vec<String>,
    /// Fields the user explicitly prohibited for this capture.
    pub denied_fields: Vec<String>,
    /// A named target/field could not be attributed to this reference.
    pub ambiguous: bool,
    /// Exact uses a person approved in the approval UI.
    pub approved_uses: Vec<SecretExactUse>,
    /// Exact uses a person refused in the approval UI.
    pub denied_uses: Vec<SecretExactUse>,
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
    /// Targets and fields the user's own messages named for this capture.
    /// Empty until a message names one; never inferred from the model.
    scope_targets: std::collections::BTreeSet<String>,
    scope_fields: std::collections::BTreeSet<String>,
    /// Explicit prohibitions. They win over a positive mention of the same
    /// name and apply in every permission mode.
    denied_targets: std::collections::BTreeSet<String>,
    denied_fields: std::collections::BTreeSet<String>,
    /// A named target/field could not be attributed to this reference.
    scope_ambiguous: bool,
    /// Exact uses the person approved or refused in the approval UI.
    approved_uses: std::collections::BTreeSet<SecretExactUse>,
    denied_uses: std::collections::BTreeSet<SecretExactUse>,
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
                scope_targets: std::collections::BTreeSet::new(),
                scope_fields: std::collections::BTreeSet::new(),
                denied_targets: std::collections::BTreeSet::new(),
                denied_fields: std::collections::BTreeSet::new(),
                scope_ambiguous: false,
                approved_uses: std::collections::BTreeSet::new(),
                denied_uses: std::collections::BTreeSet::new(),
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

    /// Record the targets and fields the user's own message named for this
    /// capture. Returns whether the reference was live and belonged to
    /// `session`.
    ///
    /// The caller supplies values already normalized by the text layer. Empty
    /// and oversized values are ignored rather than stored; a live capture
    /// with nothing to add still answers `true`, because authorization itself
    /// did not fail. Revoked and expired captures cannot gain new uses.
    pub fn authorize_use(
        &self,
        session: &str,
        reference: &SecretRef,
        targets: &[String],
        fields: &[String],
    ) -> bool {
        let declaration = leveler_core::DotenvUseDeclaration {
            targets: targets.to_vec(),
            fields: fields.to_vec(),
            ..Default::default()
        };
        self.authorize_declaration(session, reference, &declaration)
    }

    /// Record one message's structured use declaration against this capture.
    ///
    /// A prohibition always removes a matching positive grant, so a denial
    /// wins inside one message. A prohibition is not lifted by a later bare
    /// mention: the user ended that destination for this capture. The way back
    /// is a new capture or an explicit approval of the exact use in the
    /// consent surface. `ambiguous` is sticky: an explicit later grant still
    /// authorizes the matching use, but an unattributed choice remains unbound.
    pub fn authorize_declaration(
        &self,
        session: &str,
        reference: &SecretRef,
        declaration: &leveler_core::DotenvUseDeclaration,
    ) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(reference) else {
            return false;
        };
        if entry.session != session {
            return false;
        }
        if matches!(entry.state, SecretState::Revoked | SecretState::Expired) {
            return false;
        }
        if self.is_expired(entry) {
            entry.state = SecretState::Expired;
            entry.value.clear();
            return false;
        }
        for target in &declaration.targets {
            if !target.is_empty()
                && target.len() <= 1024
                && entry.scope_targets.len() < 32
                && !entry.denied_targets.contains(target)
            {
                entry.scope_targets.insert(target.clone());
            }
        }
        for field in &declaration.fields {
            if !field.is_empty()
                && field.len() <= 128
                && entry.scope_fields.len() < 32
                && !entry.denied_fields.contains(field)
            {
                entry.scope_fields.insert(field.clone());
            }
        }
        for target in &declaration.denied_targets {
            if !target.is_empty() && target.len() <= 1024 && entry.denied_targets.len() < 32 {
                entry.scope_targets.remove(target);
                entry.denied_targets.insert(target.clone());
            }
        }
        for field in &declaration.denied_fields {
            if !field.is_empty() && field.len() <= 128 && entry.denied_fields.len() < 32 {
                entry.scope_fields.remove(field);
                entry.denied_fields.insert(field.clone());
            }
        }
        if declaration.ambiguous {
            entry.scope_ambiguous = true;
        }
        true
    }

    /// Record that the person approved this exact `(target, field)` use for
    /// this capture in the approval UI. A fresh approval lifts a matching
    /// earlier refusal, but it never becomes a target-wide grant.
    pub fn record_exact_approval(
        &self,
        session: &str,
        reference: &SecretRef,
        target: &str,
        field: &str,
    ) -> bool {
        self.record_exact_decision(session, reference, target, field, true)
    }

    /// Record that the person refused this exact `(target, field)` use. A
    /// retry that asks again must be approved by the person; it can never
    /// proceed because an earlier call was denied.
    pub fn record_exact_denial(
        &self,
        session: &str,
        reference: &SecretRef,
        target: &str,
        field: &str,
    ) -> bool {
        self.record_exact_decision(session, reference, target, field, false)
    }

    fn record_exact_decision(
        &self,
        session: &str,
        reference: &SecretRef,
        target: &str,
        field: &str,
        approved: bool,
    ) -> bool {
        if target.is_empty() || target.len() > 1024 || field.is_empty() || field.len() > 128 {
            return false;
        }
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(reference) else {
            return false;
        };
        if entry.session != session {
            return false;
        }
        if matches!(entry.state, SecretState::Revoked | SecretState::Expired) {
            return false;
        }
        if self.is_expired(entry) {
            entry.state = SecretState::Expired;
            entry.value.clear();
            return false;
        }
        let use_ = SecretExactUse {
            target: target.to_string(),
            field: field.to_string(),
        };
        if approved {
            if entry.approved_uses.len() < 32 {
                entry.denied_uses.remove(&use_);
                entry.approved_uses.insert(use_);
            }
        } else if entry.denied_uses.len() < 32 {
            entry.approved_uses.remove(&use_);
            entry.denied_uses.insert(use_);
        }
        true
    }

    /// The use scope accumulated for one capture. The same availability
    /// answers as [`Self::with_value`] apply: unknown and foreign references
    /// are indistinguishable.
    pub fn use_scope(
        &self,
        session: &str,
        reference: &SecretRef,
    ) -> Result<SecretUseScope, VaultError> {
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
        Ok(SecretUseScope {
            targets: entry.scope_targets.iter().cloned().collect(),
            fields: entry.scope_fields.iter().cloned().collect(),
            denied_targets: entry.denied_targets.iter().cloned().collect(),
            denied_fields: entry.denied_fields.iter().cloned().collect(),
            ambiguous: entry.scope_ambiguous,
            approved_uses: entry.approved_uses.iter().cloned().collect(),
            denied_uses: entry.denied_uses.iter().cloned().collect(),
        })
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
