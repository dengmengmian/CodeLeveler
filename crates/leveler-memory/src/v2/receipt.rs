//! Trust receipts for records written by this store.
//!
//! A receipt proves only that a record with this revision and digest was once
//! committed by the trusted write path. It does not prove who edited an
//! external file, and it never has to be treated as an authority by itself.

use serde::{Deserialize, Serialize};

use super::frontmatter::{DocumentError, sha256_hex_text};
use super::record::{MemoryAuthority, MemoryId, MemoryRecord, MemoryScope, MemoryStatus, PinOwner};

pub const RECEIPT_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    ExternalUnverified,
    ExternalConflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub schema: u32,
    pub id: MemoryId,
    pub scope: MemoryScope,
    pub revision: u64,
    pub digest: String,
    pub status: MemoryStatus,
    pub subject_key_sha256: Option<String>,
    pub authority: MemoryAuthority,
    pub pinned_by: Option<PinOwner>,
    pub updated_at: String,
}

impl Receipt {
    pub fn from_record(record: &MemoryRecord, digest: &str) -> Self {
        Self {
            schema: RECEIPT_SCHEMA,
            id: record.frontmatter.id.clone(),
            scope: record.frontmatter.scope,
            revision: record.frontmatter.revision,
            digest: digest.to_string(),
            status: record.frontmatter.status,
            subject_key_sha256: record
                .frontmatter
                .subject_key
                .as_deref()
                .map(sha256_hex_text),
            authority: record.frontmatter.authority,
            pinned_by: record.frontmatter.pinned_by,
            updated_at: record.frontmatter.updated_at.to_rfc3339(),
        }
    }

    pub fn to_json(&self) -> Result<Vec<u8>, DocumentError> {
        serde_json::to_vec(self).map_err(|error| DocumentError::Invalid(error.to_string()))
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, DocumentError> {
        serde_json::from_slice(bytes).map_err(|error| DocumentError::Invalid(error.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct TrustView {
    pub trust: Trust,
    pub effective_authority: MemoryAuthority,
    pub protected: bool,
    pub receipt: Option<Receipt>,
}

/// Classify a current file against the receipt written at its last trusted
/// commit. A third state (revision/status/subject changed) is a conflict; a
/// body/title/value change is unverified, not adopted.
pub fn classify(record: &MemoryRecord, digest: &str, receipt: Option<&Receipt>) -> TrustView {
    let Some(receipt) = receipt else {
        return unverified(None);
    };
    if receipt.schema != RECEIPT_SCHEMA
        || receipt.id != record.frontmatter.id
        || receipt.scope != record.frontmatter.scope
    {
        return unverified(Some(receipt.clone()));
    }
    let subject_matches = receipt.subject_key_sha256
        == record
            .frontmatter
            .subject_key
            .as_deref()
            .map(sha256_hex_text);
    if receipt.revision != record.frontmatter.revision
        || receipt.status != record.frontmatter.status
        || !subject_matches
    {
        return TrustView {
            trust: Trust::ExternalConflict,
            effective_authority: MemoryAuthority::ModelInferred,
            protected: false,
            receipt: Some(receipt.clone()),
        };
    }
    if receipt.digest != digest {
        return unverified(Some(receipt.clone()));
    }
    TrustView {
        trust: Trust::Trusted,
        effective_authority: record.frontmatter.authority,
        protected: super::lifecycle::is_protected(record.frontmatter.authority),
        receipt: Some(receipt.clone()),
    }
}

fn unverified(receipt: Option<Receipt>) -> TrustView {
    TrustView {
        trust: Trust::ExternalUnverified,
        effective_authority: MemoryAuthority::ModelInferred,
        protected: false,
        receipt,
    }
}
