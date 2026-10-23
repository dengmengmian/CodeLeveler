//! Which durable-memory lanes a turn may read or write.
//!
//! User facts and agent experience share one store and one lifecycle, but they
//! are separate product capabilities. This value is the single answer to
//! "which lanes are effective right now"; it filters reads, tools and
//! background work without creating a second store or status model.
//!
//! `enabled` on the project config is the total switch. A lane is effective
//! only when BOTH the total switch and its own switch are on.

use serde::{Deserialize, Serialize};

use crate::{MemoryCandidate, MemoryEntry, MemoryKind};

/// The independently controllable halves of durable memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryLanes {
    /// Preferences, decisions, notes and other user facts.
    pub user: bool,
    /// Evidence-bound lessons learned from completed tasks.
    pub experience: bool,
}

impl MemoryLanes {
    pub const ALL: Self = Self {
        user: true,
        experience: true,
    };
    pub const USER_ONLY: Self = Self {
        user: true,
        experience: false,
    };
    pub const EXPERIENCE_ONLY: Self = Self {
        user: false,
        experience: true,
    };
    pub const NONE: Self = Self {
        user: false,
        experience: false,
    };

    pub fn any(self) -> bool {
        self.user || self.experience
    }

    /// Whether an entry of this kind belongs to an enabled lane.
    pub fn allows(self, kind: MemoryKind) -> bool {
        match kind {
            MemoryKind::Experience => self.experience,
            MemoryKind::Preference
            | MemoryKind::Decision
            | MemoryKind::Note
            | MemoryKind::LegacyUnknown
            | MemoryKind::Derived => self.user,
        }
    }

    pub fn allows_entry(self, entry: &MemoryEntry) -> bool {
        self.allows(MemoryKind::of(entry))
    }

    /// Whether a lane-kind view may be shown. Used where the view already
    /// carries its kind, so the caller does not need a synthetic entry.
    pub fn allows_experience(self, experience: bool) -> bool {
        if experience {
            self.experience
        } else {
            self.user
        }
    }

    /// A candidate is experience exactly when it carries the structured
    /// experience record; every other candidate is a user fact proposal.
    pub fn allows_candidate(self, candidate: &MemoryCandidate) -> bool {
        if candidate.experience.is_some() {
            self.experience
        } else {
            self.user
        }
    }
}

impl Default for MemoryLanes {
    fn default() -> Self {
        Self::ALL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CandidateKind, CandidateSource};

    fn entry(kind: Option<&str>) -> MemoryEntry {
        let mut entry = crate::new_entry("title", "body", Vec::new());
        entry.kind = kind.map(str::to_string);
        entry
    }

    #[test]
    fn an_unlabelled_legacy_entry_is_a_user_fact() {
        assert!(MemoryLanes::USER_ONLY.allows_entry(&entry(None)));
        assert!(!MemoryLanes::EXPERIENCE_ONLY.allows_entry(&entry(None)));
    }

    #[test]
    fn experience_is_only_in_the_experience_lane() {
        assert!(!MemoryLanes::USER_ONLY.allows_entry(&entry(Some("experience"))));
        assert!(MemoryLanes::EXPERIENCE_ONLY.allows_entry(&entry(Some("experience"))));
        assert!(MemoryLanes::ALL.allows_entry(&entry(Some("experience"))));
    }

    #[test]
    fn preferences_decisions_and_notes_are_user_facts() {
        for kind in ["preference", "decision", "note"] {
            assert!(MemoryLanes::USER_ONLY.allows_entry(&entry(Some(kind))));
            assert!(!MemoryLanes::EXPERIENCE_ONLY.allows_entry(&entry(Some(kind))));
        }
    }

    #[test]
    fn a_candidate_with_an_experience_record_is_experience() {
        let mut candidate = MemoryCandidate::new(
            "lesson",
            "body",
            CandidateKind::Free,
            None,
            CandidateSource::RuntimeObserved,
            Vec::new(),
        )
        .unwrap();
        candidate.experience = Some(crate::ExperienceRecord {
            category: crate::ExperienceCategory::RootCause,
            verification: crate::ExperienceVerification::Verified,
            applies_when: "when the test runs".to_string(),
            invalid_if: None,
            evidence: Vec::new(),
        });
        assert!(MemoryLanes::EXPERIENCE_ONLY.allows_candidate(&candidate));
        assert!(!MemoryLanes::USER_ONLY.allows_candidate(&candidate));

        candidate.experience = None;
        assert!(MemoryLanes::USER_ONLY.allows_candidate(&candidate));
        assert!(!MemoryLanes::EXPERIENCE_ONLY.allows_candidate(&candidate));
    }

    #[test]
    fn no_lane_is_not_any_lane() {
        assert!(!MemoryLanes::NONE.any());
        assert!(MemoryLanes::ALL.any());
        assert!(MemoryLanes::USER_ONLY.any());
        assert!(MemoryLanes::EXPERIENCE_ONLY.any());
    }
}
