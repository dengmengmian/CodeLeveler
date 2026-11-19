//! A read-only snapshot of the trust store for one project, and the decision
//! one definition's current digest receives against it.
//!
//! One view is loaded at a boundary (turn start, registry construction,
//! `doctor`, a trust command). It never re-reads the store behind the caller's
//! back: a mid-turn approval or edit cannot change a decision already made, and
//! a stale view can only deny, never grant.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::store::{DefinitionTrustStore, StoreStatus};
use crate::{
    DefinitionIdentity, DefinitionScope, USER_DOMAIN, project_domain, store_is_outside_project,
};

/// What the gate says about one definition's exact current bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionTrust {
    /// These exact bytes were approved by a human.
    Trusted { approval_version: u64 },
    /// No record for this identity, or no record at all.
    Unapproved,
    /// A record exists, but its digest is not the current bytes'.
    Changed { approved_digest: String },
    /// The store is corrupt, from a newer schema, or sits where the project
    /// could rewrite it. Fail closed: nothing is trusted.
    StoreUnavailable { status: StoreStatus },
    /// Compiled into the binary; this crate is not the authority.
    Builtin,
}

impl DefinitionTrust {
    pub fn is_trusted(&self) -> bool {
        matches!(self, Self::Trusted { .. })
    }

    /// Stable audit code. Not natural-language prose.
    pub fn audit_state(&self) -> &'static str {
        match self {
            Self::Trusted { .. } => "trusted",
            Self::Unapproved => "unapproved",
            Self::Changed { .. } => "changed",
            Self::StoreUnavailable { .. } => "store_unavailable",
            Self::Builtin => "builtin",
        }
    }

    pub fn approved_digest(&self) -> Option<&str> {
        match self {
            Self::Changed { approved_digest } => Some(approved_digest),
            _ => None,
        }
    }

    /// Whether the definition may enter the model's control context.
    pub fn admits(&self) -> bool {
        matches!(self, Self::Trusted { .. } | Self::Builtin)
    }
}

/// One decision as a serializable audit record. Bodies are never included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustAuditEntry {
    pub id: String,
    pub display: String,
    pub scope: String,
    pub kind: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_status: Option<String>,
}

/// One project's view of the trust store.
#[derive(Debug, Clone)]
pub struct DefinitionTrustView {
    store: DefinitionTrustStore,
    project_domain: Option<String>,
    /// The store cannot be protected from this project's sandboxed execution.
    inside_project: bool,
    /// Test-only admission of every definition. Only compiled with the
    /// `test-support` feature, which production crates do not enable.
    #[cfg(feature = "test-support")]
    allow_all: bool,
}

impl DefinitionTrustView {
    /// Load the store once for `project_root`.
    ///
    /// When the Leveler home resolves inside the project, the project's own
    /// confined shell could rewrite the store, so the view denies every
    /// definition with `store_inside_project`. That is a configuration error
    /// reported by the caller, not a reason to fall back to path trust.
    pub fn load(leveler_home: &Path, project_root: Option<&Path>) -> Self {
        let inside_project =
            project_root.is_some_and(|root| !store_is_outside_project(leveler_home, root));
        let store = if leveler_home.as_os_str().is_empty() {
            DefinitionTrustStore::empty()
        } else {
            DefinitionTrustStore::load(leveler_home)
        };
        Self {
            store,
            project_domain: project_root.map(project_domain),
            inside_project,
            #[cfg(feature = "test-support")]
            allow_all: false,
        }
    }

    /// A view that trusts no external definition. Safe default for callers
    /// with no store (unit tests of built-ins, no-workspace sessions).
    pub fn deny_all() -> Self {
        Self {
            store: DefinitionTrustStore::empty(),
            project_domain: None,
            inside_project: false,
            #[cfg(feature = "test-support")]
            allow_all: false,
        }
    }

    /// Admit every definition. Compiled only for test builds; production
    /// crates never enable `test-support`. Tests that assert the gate itself
    /// use [`Self::deny_all`] or a real store instead.
    #[cfg(feature = "test-support")]
    pub fn trust_everything_for_tests() -> Self {
        Self {
            store: DefinitionTrustStore::empty(),
            project_domain: None,
            inside_project: false,
            allow_all: true,
        }
    }

    /// The effective status: a store the project can rewrite is treated as
    /// unavailable even though the file itself parsed.
    pub fn status(&self) -> StoreStatus {
        if self.inside_project {
            return StoreStatus::InsideProject;
        }
        self.store.status().clone()
    }

    pub fn store(&self) -> &DefinitionTrustStore {
        &self.store
    }

    /// Decide one definition by its exact current digest.
    pub fn check(&self, identity: &DefinitionIdentity, digest: &str) -> DefinitionTrust {
        if identity.scope == DefinitionScope::Builtin {
            return DefinitionTrust::Builtin;
        }
        #[cfg(feature = "test-support")]
        if self.allow_all {
            return DefinitionTrust::Trusted {
                approval_version: 0,
            };
        }
        if self.inside_project {
            return DefinitionTrust::StoreUnavailable {
                status: StoreStatus::InsideProject,
            };
        }
        match self.store.status() {
            StoreStatus::Healthy | StoreStatus::Missing => {}
            status @ (StoreStatus::Corrupt { .. }
            | StoreStatus::UnsupportedVersion { .. }
            | StoreStatus::InsideProject) => {
                return DefinitionTrust::StoreUnavailable {
                    status: status.clone(),
                };
            }
        }
        let domain = match identity.scope {
            DefinitionScope::Project => {
                let Some(domain) = self.project_domain.as_deref() else {
                    return DefinitionTrust::Unapproved;
                };
                domain
            }
            DefinitionScope::User => USER_DOMAIN,
            DefinitionScope::Builtin => return DefinitionTrust::Builtin,
        };
        match self.store.approval_record(domain, identity) {
            Some(record) if record.digest == digest => DefinitionTrust::Trusted {
                approval_version: record.approval_version,
            },
            Some(record) => DefinitionTrust::Changed {
                approved_digest: record.digest.clone(),
            },
            None => DefinitionTrust::Unapproved,
        }
    }

    /// The same decision, flattened for a durable audit event.
    pub fn audit(&self, identity: &DefinitionIdentity, digest: &str) -> TrustAuditEntry {
        let trust = self.check(identity, digest);
        TrustAuditEntry {
            id: identity.id.clone(),
            display: identity.display.clone(),
            scope: identity.scope.as_str().to_string(),
            kind: identity.kind.as_str().to_string(),
            state: trust.audit_state().to_string(),
            approved_digest: trust.approved_digest().map(str::to_string),
            store_status: matches!(
                trust,
                DefinitionTrust::StoreUnavailable { .. } | DefinitionTrust::Unapproved
            )
            .then(|| self.status().as_str().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ApprovalRequest, ApprovalSurface, approve};
    use crate::{DefinitionKind, project_domain};
    use tempfile::tempdir;

    fn identity() -> DefinitionIdentity {
        DefinitionIdentity::project(DefinitionKind::RootAgents, "AGENTS.md").unwrap()
    }

    #[test]
    fn missing_store_denies_and_names_the_state() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let view = DefinitionTrustView::load(home.path(), Some(project.path()));
        assert_eq!(
            view.check(&identity(), &DefinitionIdentity::single_digest(b"A")),
            DefinitionTrust::Unapproved
        );
        assert_eq!(view.audit(&identity(), "sha256:x").state, "unapproved");
    }

    #[test]
    fn approved_bytes_are_trusted_and_other_bytes_are_changed() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        approve(
            home.path(),
            project.path(),
            &[ApprovalRequest::single(identity(), b"OLD")],
            ApprovalSurface::Cli,
        )
        .unwrap();
        let view = DefinitionTrustView::load(home.path(), Some(project.path()));
        assert!(matches!(
            view.check(&identity(), &DefinitionIdentity::single_digest(b"OLD")),
            DefinitionTrust::Trusted { .. }
        ));
        assert_eq!(
            view.check(&identity(), &DefinitionIdentity::single_digest(b"NEW")),
            DefinitionTrust::Changed {
                approved_digest: DefinitionIdentity::single_digest(b"OLD")
            }
        );
    }

    #[test]
    fn approvals_do_not_leak_across_projects() {
        let home = tempdir().unwrap();
        let trusted = tempdir().unwrap();
        let other = tempdir().unwrap();
        approve(
            home.path(),
            trusted.path(),
            &[ApprovalRequest::single(identity(), b"A")],
            ApprovalSurface::Cli,
        )
        .unwrap();
        let view = DefinitionTrustView::load(home.path(), Some(other.path()));
        assert_eq!(
            view.check(&identity(), &DefinitionIdentity::single_digest(b"A")),
            DefinitionTrust::Unapproved
        );
        assert_ne!(project_domain(trusted.path()), project_domain(other.path()));
    }

    #[test]
    fn user_scope_is_one_global_domain() {
        let home = tempdir().unwrap();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let user_agent = DefinitionIdentity::user_agent("reviewer").unwrap();
        approve(
            home.path(),
            first.path(),
            &[ApprovalRequest::single(user_agent.clone(), b"AGENT")],
            ApprovalSurface::Cli,
        )
        .unwrap();
        let view = DefinitionTrustView::load(home.path(), Some(second.path()));
        assert!(matches!(
            view.check(&user_agent, &DefinitionIdentity::single_digest(b"AGENT")),
            DefinitionTrust::Trusted { .. }
        ));
    }

    #[test]
    fn a_corrupt_store_denies_everything() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        std::fs::write(crate::store_path(home.path()), "!! not yaml !!").unwrap();
        let view = DefinitionTrustView::load(home.path(), Some(project.path()));
        assert!(matches!(
            view.check(&identity(), "sha256:any"),
            DefinitionTrust::StoreUnavailable {
                status: StoreStatus::Corrupt { .. }
            }
        ));
        assert!(matches!(view.status(), StoreStatus::Corrupt { .. }));
    }

    #[test]
    fn a_home_inside_the_project_denies_everything() {
        let project = tempdir().unwrap();
        let home = project.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let view = DefinitionTrustView::load(&home, Some(project.path()));
        assert_eq!(view.status(), StoreStatus::InsideProject);
        assert_eq!(
            view.check(&identity(), "sha256:any"),
            DefinitionTrust::StoreUnavailable {
                status: StoreStatus::InsideProject
            }
        );
    }
}
