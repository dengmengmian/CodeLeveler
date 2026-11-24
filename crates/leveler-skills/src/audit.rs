//! What one turn's skill delivery actually did.
//!
//! This is a delivery record, not a compliance receipt. It distinguishes the
//! skills the index made discoverable, the ones the user's message selected,
//! the ones whose package was loaded into the turn context, and every refusal
//! or host block that kept a selection out. Nothing here proves the model
//! followed a procedure it was given.
//!
//! The record never carries a skill body, reference text, or a filesystem
//! location. Names, provenance, sizes, and stable failure codes are enough for
//! an operator to compare a selection with the registry without turning the
//! audit log into a second copy of the prompt.

use serde::{Deserialize, Serialize};

use crate::registry::{SkillDetail, SkillLookupError, SkillScope, SkillSource};

/// Which explicit surface the user's message used to select a skill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDeliverySurface {
    /// No explicit selection; only the model-facing index was delivered.
    #[default]
    IndexOnly,
    /// A leading `/name [args]`.
    Slash,
    /// One or more `$name` mentions.
    Mention,
    /// Both a slash command and a mention.
    Mixed,
}

/// Why a selected skill did not reach the turn context.
///
/// These are stable audit codes. The human-readable error stays in the local
/// log and in the turn's refusal notice; it can name a host path, so it is not
/// part of the durable record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDeliveryFailure {
    /// The name is not in the registry.
    NotFound,
    /// The package exists but its definition is invalid.
    Invalid,
    /// The user turned the skill off.
    Disabled,
    /// The exact package bytes are not approved.
    Untrusted,
    /// The package opted out of `/` and `$` invocation.
    UserInvocationDisabled,
    /// The package opted out of model discovery and `load_skill`.
    ModelInvocationDisabled,
    /// The package could not be read.
    Read,
    /// The requested scope:name is owned by a higher-precedence definition.
    Shadowed,
    /// The host did not expose or allow the Skills capability this turn.
    CapabilityUnavailable,
}

impl SkillDeliveryFailure {
    /// The stable code used by logs and clients.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Invalid => "invalid",
            Self::Disabled => "disabled",
            Self::Untrusted => "untrusted",
            Self::UserInvocationDisabled => "user_invocation_disabled",
            Self::ModelInvocationDisabled => "model_invocation_disabled",
            Self::Read => "read",
            Self::Shadowed => "shadowed",
            Self::CapabilityUnavailable => "capability_unavailable",
        }
    }
}

/// What a delivery record proves about the model's behaviour. Delivery is a
/// host fact; following a procedure is not mechanically checkable here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillUseEvidence {
    /// The record proves the package reached the context, nothing more.
    #[default]
    NotVerifiable,
}

/// One requested skill and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDeliveryEntry {
    /// Canonical registry name.
    pub name: String,
    /// The token(s) the user typed: `/deploy`, `$deploy`, or both joined by a
    /// space when one message used both surfaces for the same skill.
    pub requested_as: String,
    /// Registry provenance, when the entry resolved to a package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<SkillScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SkillSource>,
    /// Procedure body bytes, or zero when nothing was loaded.
    pub bytes: usize,
    /// `None` means the package was loaded into the turn context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<SkillDeliveryFailure>,
}

impl SkillDeliveryEntry {
    pub fn loaded(&self) -> bool {
        self.failure.is_none()
    }
}

/// The durable delivery audit for one turn.
///
/// `injected_bytes` is the size of the whole `selected_skills` block; the
/// per-entry `bytes` is that skill's procedure body. `index_bytes` is what the
/// model-facing catalog cost, and `discovered` is how many entries that
/// catalog advertised.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDeliveryAudit {
    pub surface: SkillDeliverySurface,
    pub discovered: usize,
    pub index_bytes: usize,
    pub entries: Vec<SkillDeliveryEntry>,
    pub injected_bytes: usize,
    /// Set when the host refused the whole delivery, independent of any one
    /// name. A blocked turn has no loaded entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<SkillDeliveryFailure>,
    /// Fixed marker: this audit never claims the model followed a procedure.
    /// Absent in older rows, which read as `NotVerifiable`.
    #[serde(default)]
    pub agent_use: SkillUseEvidence,
}

impl SkillDeliveryAudit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record how many skills the model-facing index advertised and what the
    /// delivered text cost. Zero bytes means the index was not delivered.
    pub fn discover(&mut self, advertised: usize, index_bytes: usize) {
        self.discovered = advertised;
        self.index_bytes = index_bytes;
    }

    /// Record that the host refused the whole delivery.
    pub fn block(&mut self, failure: SkillDeliveryFailure) {
        self.blocked = Some(failure);
    }

    /// Record a successfully loaded package. A name selected twice through two
    /// surfaces stays one entry.
    pub fn record_loaded(&mut self, requested_as: &str, detail: &SkillDetail) {
        self.record(SkillDeliveryEntry {
            name: detail.name.clone(),
            requested_as: requested_as.to_string(),
            scope: Some(detail.scope),
            source: Some(detail.source),
            bytes: detail.body.len(),
            failure: None,
        });
    }

    /// Record a refusal for a known or requested name.
    pub fn record_failure(
        &mut self,
        requested_as: &str,
        name: &str,
        failure: SkillDeliveryFailure,
    ) {
        self.record(SkillDeliveryEntry {
            name: name.to_string(),
            requested_as: requested_as.to_string(),
            scope: None,
            source: None,
            bytes: 0,
            failure: Some(failure),
        });
    }

    /// Record a refusal that came from the registry, keeping its provenance.
    pub fn record_lookup_error(&mut self, requested_as: &str, error: &SkillLookupError) {
        let (name, scope, source, failure) = describe(error);
        self.record(SkillDeliveryEntry {
            name,
            requested_as: requested_as.to_string(),
            scope,
            source,
            bytes: 0,
            failure: Some(failure),
        });
    }

    /// Close the audit with the rendered injection size.
    pub fn finish(&mut self, injected: Option<&str>) {
        self.injected_bytes = injected.map(str::len).unwrap_or(0);
    }

    fn record(&mut self, entry: SkillDeliveryEntry) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.name == entry.name) {
            if !entry.requested_as.is_empty()
                && !existing
                    .requested_as
                    .split(' ')
                    .any(|t| t == entry.requested_as)
            {
                existing.requested_as.push(' ');
                existing.requested_as.push_str(&entry.requested_as);
            }
            // A success after a failure for the same name is the real outcome:
            // the package did reach this turn.
            if entry.failure.is_none() {
                existing.failure = None;
                existing.scope = entry.scope;
                existing.source = entry.source;
                existing.bytes = entry.bytes;
            }
            return;
        }
        self.entries.push(entry);
    }
}

/// The stable code for a lookup refusal.
pub(crate) fn lookup_failure(error: &SkillLookupError) -> SkillDeliveryFailure {
    match error {
        SkillLookupError::NotFound { .. } => SkillDeliveryFailure::NotFound,
        SkillLookupError::Invalid { .. } => SkillDeliveryFailure::Invalid,
        SkillLookupError::Disabled { .. } => SkillDeliveryFailure::Disabled,
        SkillLookupError::ModelInvocationDisabled { .. } => {
            SkillDeliveryFailure::ModelInvocationDisabled
        }
        SkillLookupError::UserInvocationDisabled { .. } => {
            SkillDeliveryFailure::UserInvocationDisabled
        }
        SkillLookupError::Read { .. } => SkillDeliveryFailure::Read,
        SkillLookupError::Untrusted { .. } => SkillDeliveryFailure::Untrusted,
    }
}

/// The canonical name, provenance, and stable failure code for a lookup error.
fn describe(
    error: &SkillLookupError,
) -> (
    String,
    Option<SkillScope>,
    Option<SkillSource>,
    SkillDeliveryFailure,
) {
    let failure = lookup_failure(error);
    match error {
        SkillLookupError::NotFound { name, .. } => (name.clone(), None, None, failure),
        SkillLookupError::Invalid {
            name,
            scope,
            source,
            ..
        }
        | SkillLookupError::Disabled {
            name,
            scope,
            source,
            ..
        }
        | SkillLookupError::ModelInvocationDisabled {
            name,
            scope,
            source,
        }
        | SkillLookupError::UserInvocationDisabled {
            name,
            scope,
            source,
        }
        | SkillLookupError::Untrusted {
            name,
            scope,
            source,
            ..
        } => (name.clone(), Some(*scope), Some(*source), failure),
        SkillLookupError::Read { name, .. } => (name.clone(), None, None, failure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail() -> SkillDetail {
        SkillDetail {
            name: "deploy".into(),
            description: "Deploy.".into(),
            body: "deploy-procedure-body-marker".into(),
            scope: SkillScope::Project,
            source: SkillSource::Native,
            dir: std::path::PathBuf::from("/private/host/path"),
            scripts: Vec::new(),
            references: Vec::new(),
            reference_grant: None,
            other_files: Vec::new(),
        }
    }

    #[test]
    fn a_loaded_entry_records_size_and_provenance_but_never_the_body() {
        let mut audit = SkillDeliveryAudit::new();
        audit.record_loaded("/deploy", &detail());
        audit.finish(Some("rendered injection"));
        let json = serde_yaml::to_string(&audit).unwrap();
        assert!(!json.contains("deploy-procedure-body-marker"));
        assert!(!json.contains("/private/host/path"));
        assert_eq!(audit.entries.len(), 1);
        assert_eq!(audit.entries[0].scope, Some(SkillScope::Project));
        assert_eq!(audit.entries[0].source, Some(SkillSource::Native));
        assert_eq!(audit.entries[0].bytes, detail().body.len());
        assert_eq!(audit.injected_bytes, "rendered injection".len());
        assert!(audit.entries[0].loaded());
    }

    #[test]
    fn both_surfaces_for_one_skill_stay_one_entry() {
        let mut audit = SkillDeliveryAudit::new();
        audit.record_loaded("/deploy", &detail());
        audit.record_loaded("$deploy", &detail());
        assert_eq!(audit.entries.len(), 1);
        assert_eq!(audit.entries[0].requested_as, "/deploy $deploy");
    }

    #[test]
    fn a_lookup_failure_keeps_its_stable_code_and_provenance() {
        let mut audit = SkillDeliveryAudit::new();
        audit.record_lookup_error(
            "$deploy",
            &SkillLookupError::Disabled {
                name: "deploy".into(),
                scope: SkillScope::User,
                source: SkillSource::Claude,
                reason: "turned off".into(),
            },
        );
        assert_eq!(
            audit.entries[0].failure,
            Some(SkillDeliveryFailure::Disabled)
        );
        assert_eq!(audit.entries[0].scope, Some(SkillScope::User));
        assert!(!audit.entries[0].loaded());
    }
}
