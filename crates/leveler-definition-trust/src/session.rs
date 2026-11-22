//! Session-scoped definition trust.
//!
//! The persistent store answers "did a human approve these exact bytes at some
//! point". Two additional decisions can only exist for the lifetime of one
//! session:
//!
//! - a user allowance the person granted while a turn was running ("use this
//!   skill this time"), which must never reach the store;
//! - FullAccess adoption of the project definitions that existed before this
//!   session's agent could write anything.
//!
//! Both are content-bound: a grant names one identity and one exact digest.
//! Any edit produces a different digest and the grant stops applying, which is
//! the same invalidation rule the persistent store follows.
//!
//! The baseline is captured ONCE, when the session is created, before any
//! model call or tool execution. A definition the agent writes after that is
//! therefore never equal to its baseline entry. A process that resumes an
//! existing session has no such evidence and must not auto-adopt anything.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::{DefinitionIdentity, DefinitionTrust, discover_project};

/// Why one digest is admitted for the current session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SessionTrustSource {
    /// The user approved this exact content for this session only.
    AllowOnce,
    /// The user chose to persist the approval; the same digest is also
    /// admitted for the rest of this session without re-reading the store.
    TrustCurrent,
    /// FullAccess adopted content that predates this session's agent.
    FullAdoption,
}

impl SessionTrustSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::TrustCurrent => "trust_current",
            Self::FullAdoption => "full_adoption",
        }
    }
}

/// The project definitions and their exact digests at session creation.
///
/// Only kinds whose text is instruction/procedure content are captured. A
/// declarative agent definition can start a child with its own tool surface,
/// so it keeps requiring the persistent human approval.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDefinitionBaseline {
    entries: BTreeMap<String, String>,
}

impl SessionDefinitionBaseline {
    /// Capture every project definition that Full may adopt.
    ///
    /// Unreadable candidates are omitted: nothing can prove what their bytes
    /// were, so they are not adoptable. The walk is the same discovery the
    /// trust surface uses, so the baseline and `/trust` name the same things.
    pub fn capture(project_root: &Path) -> Self {
        let mut entries = BTreeMap::new();
        for candidate in discover_project(project_root) {
            if !candidate.identity.kind.is_full_adoptable() {
                continue;
            }
            if let Some(digest) = candidate.digest {
                entries.insert(candidate.identity.id, digest);
            }
        }
        Self { entries }
    }

    #[cfg(test)]
    pub(crate) fn from_entries_for_tests(entries: BTreeMap<String, String>) -> Self {
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn digest_for(&self, identity: &DefinitionIdentity) -> Option<&str> {
        self.entries.get(&identity.id).map(String::as_str)
    }
}

/// The baseline's provenance in THIS process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineProvenance {
    /// Captured when this process created the session, before any turn.
    CreatedThisProcess,
    /// The session already existed. Its agent may have written definitions in
    /// an earlier process, so nothing is adoptable.
    Resumed,
    /// No baseline has been captured.
    Unknown,
}

#[derive(Debug, Clone, Default)]
struct SessionTrustState {
    /// identity id -> digest -> source.
    grants: BTreeMap<String, BTreeMap<String, SessionTrustSource>>,
    /// identity id -> digests the user refused in this session. A refused
    /// request is answered from here instead of asking again.
    denials: BTreeMap<String, BTreeSet<String>>,
    baseline: Option<SessionDefinitionBaseline>,
    provenance: BaselineProvenanceState,
    full_adoption: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum BaselineProvenanceState {
    #[default]
    Unknown,
    CreatedThisProcess,
    Resumed,
}

impl From<BaselineProvenanceState> for BaselineProvenance {
    fn from(value: BaselineProvenanceState) -> Self {
        match value {
            BaselineProvenanceState::Unknown => Self::Unknown,
            BaselineProvenanceState::CreatedThisProcess => Self::CreatedThisProcess,
            BaselineProvenanceState::Resumed => Self::Resumed,
        }
    }
}

/// One session's in-memory trust decisions. Cloned through an `Arc`, so every
/// turn, tool context and sub-agent of the session observes the same grants.
#[derive(Debug)]
pub struct SessionDefinitionTrust {
    state: RwLock<SessionTrustState>,
}

impl Default for SessionDefinitionTrust {
    fn default() -> Self {
        Self::unknown()
    }
}

impl SessionDefinitionTrust {
    /// A session with no baseline evidence. Nothing is auto-adopted.
    pub fn unknown() -> Self {
        Self {
            state: RwLock::new(SessionTrustState::default()),
        }
    }

    /// A session this process created. `baseline` was captured before any
    /// model call; `full_adoption` is set only for a FullAccess creation.
    pub fn created(
        baseline: SessionDefinitionBaseline,
        provenance: BaselineProvenance,
        full_adoption: bool,
    ) -> Self {
        let state = SessionTrustState {
            grants: BTreeMap::new(),
            denials: BTreeMap::new(),
            baseline: Some(baseline),
            provenance: match provenance {
                BaselineProvenance::CreatedThisProcess => {
                    BaselineProvenanceState::CreatedThisProcess
                }
                BaselineProvenance::Resumed => BaselineProvenanceState::Resumed,
                BaselineProvenance::Unknown => BaselineProvenanceState::Unknown,
            },
            full_adoption,
        };
        Self {
            state: RwLock::new(state),
        }
    }

    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, SessionTrustState> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, SessionTrustState> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The baseline's provenance, for diagnostics and tests.
    pub fn provenance(&self) -> BaselineProvenance {
        self.read().provenance.into()
    }

    pub fn has_baseline(&self) -> bool {
        self.read()
            .baseline
            .as_ref()
            .is_some_and(|baseline| !baseline.is_empty())
    }

    /// Number of captured baseline entries. A count, never content.
    pub fn baseline_entries(&self) -> usize {
        self.read()
            .baseline
            .as_ref()
            .map_or(0, SessionDefinitionBaseline::len)
    }

    pub fn baseline_digest(&self, identity: &DefinitionIdentity) -> Option<String> {
        self.read()
            .baseline
            .as_ref()
            .and_then(|baseline| baseline.digest_for(identity))
            .map(str::to_string)
    }

    /// Whether FullAccess may adopt content for this session right now.
    ///
    /// A process that did not create the session never has the baseline
    /// evidence Full needs, so this stays false even if the mode is Full.
    pub fn full_adoption_enabled(&self) -> bool {
        let state = self.read();
        state.full_adoption
            && matches!(
                state.provenance,
                BaselineProvenanceState::CreatedThisProcess
            )
            && state
                .baseline
                .as_ref()
                .is_some_and(|baseline| !baseline.is_empty())
    }

    /// The mode owner calls this whenever the session's live permission
    /// profile changes. Enabling it for a session without a credible baseline
    /// is refused, so Full cannot be used to launder pre-existing content.
    pub fn set_full_adoption(&self, enabled: bool) {
        let mut state = self.write();
        let credible = matches!(
            state.provenance,
            BaselineProvenanceState::CreatedThisProcess
        ) && state
            .baseline
            .as_ref()
            .is_some_and(|baseline| !baseline.is_empty());
        state.full_adoption = enabled && credible;
    }

    /// Whether the exact digest is the session baseline for `identity`.
    pub fn baseline_matches(&self, identity: &DefinitionIdentity, digest: &str) -> bool {
        self.read()
            .baseline
            .as_ref()
            .and_then(|baseline| baseline.digest_for(identity))
            .is_some_and(|baseline| baseline == digest)
    }

    /// Admit one exact content version for this session only.
    pub fn allow(&self, identity: &DefinitionIdentity, digest: &str, source: SessionTrustSource) {
        let mut state = self.write();
        state
            .grants
            .entry(identity.id.clone())
            .or_default()
            .insert(digest.to_string(), source);
        // An explicit allowance supersedes an earlier refusal of the same
        // bytes; the newest human decision wins.
        if let Some(denied) = state.denials.get_mut(&identity.id) {
            denied.remove(digest);
        }
    }

    pub fn deny(&self, identity: &DefinitionIdentity, digest: &str) {
        let mut state = self.write();
        state
            .denials
            .entry(identity.id.clone())
            .or_default()
            .insert(digest.to_string());
    }

    pub fn is_denied(&self, identity: &DefinitionIdentity, digest: &str) -> bool {
        self.read()
            .denials
            .get(&identity.id)
            .is_some_and(|digests| digests.contains(digest))
    }

    /// The session-only decision for these bytes, ignoring the persistent
    /// store. Used by [`crate::DefinitionTrustView::check`] and by callers
    /// that need to ask "did this session admit these exact bytes?" without a
    /// store read.
    pub fn overlay(&self, identity: &DefinitionIdentity, digest: &str) -> Option<DefinitionTrust> {
        let state = self.read();
        if let Some(source) = state
            .grants
            .get(&identity.id)
            .and_then(|digests| digests.get(digest))
            .copied()
        {
            return Some(DefinitionTrust::SessionTrusted { source });
        }
        if state.full_adoption
            && matches!(
                state.provenance,
                BaselineProvenanceState::CreatedThisProcess
            )
            && identity.kind.is_full_adoptable()
            && state
                .baseline
                .as_ref()
                .and_then(|baseline| baseline.digest_for(identity))
                .is_some_and(|baseline| baseline == digest)
        {
            return Some(DefinitionTrust::FullAdopted);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DefinitionKind;
    use tempfile::tempdir;

    fn rule(_rel: &str) -> DefinitionIdentity {
        DefinitionIdentity::project(DefinitionKind::RuleFile, ".leveler/rules/base.md").unwrap()
    }

    fn skill(name: &str) -> DefinitionIdentity {
        DefinitionIdentity::project(DefinitionKind::Skill, &format!(".leveler/skills/{name}"))
            .unwrap()
    }

    #[test]
    fn an_allowance_is_bound_to_one_exact_digest() {
        let session = SessionDefinitionTrust::unknown();
        let identity = skill("demo");
        session.allow(&identity, "sha256:a", SessionTrustSource::AllowOnce);
        assert!(matches!(
            session.overlay(&identity, "sha256:a"),
            Some(DefinitionTrust::SessionTrusted {
                source: SessionTrustSource::AllowOnce
            })
        ));
        assert!(session.overlay(&identity, "sha256:b").is_none());
    }

    #[test]
    fn a_refusal_suppresses_only_the_refused_digest() {
        let session = SessionDefinitionTrust::unknown();
        let identity = skill("demo");
        session.deny(&identity, "sha256:a");
        assert!(session.is_denied(&identity, "sha256:a"));
        assert!(!session.is_denied(&identity, "sha256:b"));
    }

    #[test]
    fn full_adoption_requires_the_creation_baseline_and_an_adoptable_kind() {
        let mut baseline = SessionDefinitionBaseline::default();
        let rule_id = rule("AGENTS.md");
        let skill_id = skill("demo");
        let agent =
            DefinitionIdentity::project(DefinitionKind::Agent, ".leveler/agents/x").unwrap();
        baseline
            .entries
            .insert(rule_id.id.clone(), "sha256:rule".into());
        baseline
            .entries
            .insert(skill_id.id.clone(), "sha256:skill".into());
        baseline
            .entries
            .insert(agent.id.clone(), "sha256:agent".into());
        let session =
            SessionDefinitionTrust::created(baseline, BaselineProvenance::CreatedThisProcess, true);
        assert!(matches!(
            session.overlay(&rule_id, "sha256:rule"),
            Some(DefinitionTrust::FullAdopted)
        ));
        assert!(matches!(
            session.overlay(&skill_id, "sha256:skill"),
            Some(DefinitionTrust::FullAdopted)
        ));
        // A declarative agent needs the persistent human approval.
        assert!(session.overlay(&agent, "sha256:agent").is_none());
        // Edited content is not the baseline.
        assert!(session.overlay(&skill_id, "sha256:new").is_none());
    }

    #[test]
    fn a_resumed_session_has_no_adoption_evidence() {
        let mut baseline = SessionDefinitionBaseline::default();
        let rule_id = rule("AGENTS.md");
        baseline
            .entries
            .insert(rule_id.id.clone(), "sha256:rule".into());
        let session = SessionDefinitionTrust::created(baseline, BaselineProvenance::Resumed, true);
        assert!(!session.full_adoption_enabled());
        assert!(session.overlay(&rule_id, "sha256:rule").is_none());
    }

    #[test]
    fn turning_full_off_stops_adoption_without_touching_grants() {
        let mut baseline = SessionDefinitionBaseline::default();
        let rule_id = rule("AGENTS.md");
        baseline
            .entries
            .insert(rule_id.id.clone(), "sha256:rule".into());
        let session =
            SessionDefinitionTrust::created(baseline, BaselineProvenance::CreatedThisProcess, true);
        session.allow(&rule_id, "sha256:rule", SessionTrustSource::AllowOnce);
        session.set_full_adoption(false);
        assert!(!session.full_adoption_enabled());
        // The explicit allowance is a separate user decision.
        assert!(matches!(
            session.overlay(&rule_id, "sha256:rule"),
            Some(DefinitionTrust::SessionTrusted { .. })
        ));
    }

    #[test]
    fn capturing_a_project_baseline_skips_agent_definitions() {
        let project = tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".leveler/rules")).unwrap();
        std::fs::write(project.path().join(".leveler/rules/base.md"), "root").unwrap();
        std::fs::create_dir_all(project.path().join(".leveler/agents/x")).unwrap();
        std::fs::write(
            project.path().join(".leveler/agents/x/agent.yaml"),
            "name: x",
        )
        .unwrap();
        std::fs::write(
            project.path().join(".leveler/agents/x/instructions.md"),
            "do",
        )
        .unwrap();
        let baseline = SessionDefinitionBaseline::capture(project.path());
        let rule_id = rule("AGENTS.md");
        let agent =
            DefinitionIdentity::project(DefinitionKind::Agent, ".leveler/agents/x").unwrap();
        assert!(baseline.digest_for(&rule_id).is_some());
        assert!(baseline.digest_for(&agent).is_none());
    }
}
