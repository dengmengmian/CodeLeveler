//! Runtime-governed write ownership for delegated children.
//!
//! SPAWN != WRITE AUTHORITY: a normal model-spawned child starts read-capable
//! and gains scoped write authority only through an explicit, atomic
//! [`OwnershipRegistry::try_claim`]. The registry is the ONLY authoritative
//! source of active write ownership — the per-child `BackgroundChild.scope`
//! remains a UI/settlement projection, never a second truth.
//!
//! It is not a scheduler: it answers "who may write where, right now" and
//! nothing else.

use std::collections::HashMap;
use std::sync::Mutex;

/// One conflicting path in a denied claim: the path requested and the label of
/// its current owner ("parent" or "<nickname> (<id>)").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimConflict {
    pub path: String,
    pub owner: String,
}

/// Why a claim was rejected outright (before any conflict check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimRejection {
    /// Empty path list / paths that normalize to nothing.
    Empty,
    /// A path that would grant the whole workspace (".", "./", "/", "").
    RootBoundary(String),
    /// Absolute path or a path escaping the workspace via "..".
    UnsafePath(String),
    /// One or more requested paths are exclusively owned by someone else, or
    /// are actively being mutated by the parent right now.
    Conflicts(Vec<ClaimConflict>),
}

impl ClaimRejection {
    /// Honest model-facing denial text. A denial is a coordination result,
    /// never a child-fatal error.
    pub fn for_model(&self) -> String {
        match self {
            Self::Empty => "claim_write_scope needs a non-empty `paths` list naming the \
                 files or directories you intend to modify."
                .to_string(),
            Self::RootBoundary(p) => format!(
                "claim_write_scope denied: '{p}' would claim the whole workspace. \
                 Claim the specific files or directories you will modify."
            ),
            Self::UnsafePath(p) => {
                format!("claim_write_scope denied: '{p}' is not a relative in-workspace path.")
            }
            Self::Conflicts(conflicts) => {
                let lines: Vec<String> = conflicts
                    .iter()
                    .map(|c| format!("{} (owned by {})", c.path, c.owner))
                    .collect();
                format!(
                    "claim_write_scope denied — conflicting ownership: {}. This is a \
                     coordination result, not a failure: narrow your scope, work on \
                     something else, or retry after the owner settles.",
                    lines.join(", ")
                )
            }
        }
    }
}

#[derive(Default)]
struct RegistryState {
    /// owner id -> normalized exclusive paths.
    claims: HashMap<String, Vec<String>>,
    /// owner id -> display label for denials.
    labels: HashMap<String, String>,
    /// Paths the parent is mutating RIGHT NOW (admission-scoped, transient).
    /// A claim overlapping these is denied retryably (§19: claim must not
    /// race an in-flight parent mutation).
    parent_active: Vec<String>,
}

/// Shared, workspace-wide write-ownership truth. One instance per top-level
/// execution, `Arc`-shared into every child.
#[derive(Default)]
pub struct OwnershipRegistry {
    state: Mutex<RegistryState>,
}

/// `./`-stripped, trailing-slash-stripped form used for all containment math
/// (same normalization as `sub_agent::scopes_overlap` and the write
/// allowlist).
fn normalize(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn covers(owner: &str, target: &str) -> bool {
    owner == target
        || target.starts_with(&format!("{owner}/"))
        || owner.starts_with(&format!("{target}/"))
}

impl OwnershipRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a display label for an owner (used in denial messages).
    pub fn register_owner(&self, owner_id: &str, label: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.labels.insert(owner_id.to_string(), label.to_string());
    }

    /// Atomically claim `paths` for `owner_id`. All-or-nothing: on any
    /// rejection NOTHING is acquired. Paths the owner already holds are
    /// ignored (incremental claim); only genuinely new paths are evaluated.
    pub fn try_claim(
        &self,
        owner_id: &str,
        paths: &[String],
    ) -> Result<Vec<String>, ClaimRejection> {
        let mut requested: Vec<String> = Vec::new();
        for raw in paths {
            let p = normalize(raw);
            if p.is_empty() || p == "." {
                return Err(ClaimRejection::RootBoundary(raw.clone()));
            }
            if p.starts_with('/') || p.split('/').any(|seg| seg == "..") {
                return Err(ClaimRejection::UnsafePath(raw.clone()));
            }
            if !requested.contains(&p) {
                requested.push(p);
            }
        }
        if requested.is_empty() {
            return Err(ClaimRejection::Empty);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let already: Vec<String> = state.claims.get(owner_id).cloned().unwrap_or_default();
        let new_paths: Vec<String> = requested
            .into_iter()
            .filter(|p| !already.iter().any(|own| covers(own, p)))
            .collect();
        if new_paths.is_empty() {
            // Everything requested is already owned — idempotent success.
            return Ok(already);
        }
        let mut conflicts = Vec::new();
        for p in &new_paths {
            for (other, owned) in &state.claims {
                if other == owner_id {
                    continue;
                }
                if owned.iter().any(|o| covers(o, p)) {
                    let owner = state
                        .labels
                        .get(other)
                        .cloned()
                        .unwrap_or_else(|| other.clone());
                    conflicts.push(ClaimConflict {
                        path: p.clone(),
                        owner,
                    });
                }
            }
            if state.parent_active.iter().any(|a| covers(a, p)) {
                conflicts.push(ClaimConflict {
                    path: p.clone(),
                    owner: "parent (mutation in flight — retry shortly)".to_string(),
                });
            }
        }
        if !conflicts.is_empty() {
            return Err(ClaimRejection::Conflicts(conflicts));
        }
        let entry = state.claims.entry(owner_id.to_string()).or_default();
        entry.extend(new_paths);
        Ok(entry.clone())
    }

    /// Everything `owner_id` currently owns (normalized). Empty = no write
    /// authority.
    pub fn owned_by(&self, owner_id: &str) -> Vec<String> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.claims.get(owner_id).cloned().unwrap_or_default()
    }

    /// Release every claim held by `owner_id`. Idempotent; exactly-once safe.
    pub fn release_all(&self, owner_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.claims.remove(owner_id);
        state.labels.remove(owner_id);
    }

    /// Paths in `targets` owned by someone OTHER than `exclude_owner`
    /// (the parent write fence asks with `exclude_owner="parent"`).
    pub fn conflicts_for(&self, targets: &[String], exclude_owner: &str) -> Vec<ClaimConflict> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for raw in targets {
            let t = normalize(raw);
            for (owner, owned) in &state.claims {
                if owner == exclude_owner {
                    continue;
                }
                if owned.iter().any(|o| covers(o, &t)) {
                    let label = state
                        .labels
                        .get(owner)
                        .cloned()
                        .unwrap_or_else(|| owner.clone());
                    out.push(ClaimConflict {
                        path: raw.clone(),
                        owner: label,
                    });
                }
            }
        }
        out
    }

    /// Mark paths as under active parent mutation for the duration of the
    /// returned guard (claim requests overlapping them are denied retryably).
    pub fn parent_mutation_guard(&self, paths: Vec<String>) -> ParentMutationGuard<'_> {
        let normalized: Vec<String> = paths.iter().map(|p| normalize(p)).collect();
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.parent_active.extend(normalized.iter().cloned());
        }
        ParentMutationGuard {
            registry: self,
            paths: normalized,
        }
    }
}

/// RAII guard for the parent's in-flight mutation targets.
pub struct ParentMutationGuard<'a> {
    registry: &'a OwnershipRegistry,
    paths: Vec<String>,
}

impl Drop for ParentMutationGuard<'_> {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for p in &self.paths {
            if let Some(pos) = state.parent_active.iter().position(|x| x == p) {
                state.parent_active.remove(pos);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn claim_is_atomic_all_or_nothing() {
        let reg = OwnershipRegistry::new();
        reg.try_claim("a", &v(&["src/parser/"])).unwrap();
        // b requests two paths, one conflicting: NOTHING is acquired.
        let err = reg
            .try_claim("b", &v(&["tests/parser/", "src/parser/foo.rs"]))
            .unwrap_err();
        assert!(matches!(err, ClaimRejection::Conflicts(_)));
        assert!(
            reg.owned_by("b").is_empty(),
            "partial acquisition is forbidden"
        );
    }

    #[test]
    fn incremental_claim_keeps_prior_paths_and_evaluates_only_new_ones() {
        let reg = OwnershipRegistry::new();
        reg.try_claim("a", &v(&["src/parser.rs", "tests/parser.rs"]))
            .unwrap();
        let owned = reg.try_claim("a", &v(&["src/types.rs"])).unwrap();
        assert_eq!(owned.len(), 3);
        // Re-claiming an owned path is idempotent success, not a conflict.
        let owned = reg.try_claim("a", &v(&["src/parser.rs"])).unwrap();
        assert_eq!(owned.len(), 3);
    }

    #[test]
    fn root_and_unsafe_boundaries_are_hard_denied() {
        let reg = OwnershipRegistry::new();
        for bad in ["", ".", "./", "/", "/etc/passwd", "../outside", "a/../../b"] {
            let err = reg.try_claim("a", &v(&[bad])).unwrap_err();
            assert!(
                matches!(
                    err,
                    ClaimRejection::RootBoundary(_)
                        | ClaimRejection::UnsafePath(_)
                        | ClaimRejection::Empty
                ),
                "{bad:?} must be rejected, got a grant"
            );
        }
        assert!(reg.owned_by("a").is_empty());
        // Broad subtrees are NOT automatically unsafe.
        assert!(reg.try_claim("a", &v(&["src/"])).is_ok());
    }

    #[test]
    fn overlap_is_denied_by_containment_in_both_directions() {
        let reg = OwnershipRegistry::new();
        reg.register_owner("a", "Euclid (a)");
        reg.try_claim("a", &v(&["src/parser/"])).unwrap();
        // File inside owned dir.
        let err = reg.try_claim("b", &v(&["src/parser/foo.rs"])).unwrap_err();
        let ClaimRejection::Conflicts(c) = err else {
            panic!("expected conflicts")
        };
        assert_eq!(c[0].owner, "Euclid (a)");
        // Dir above an owned file.
        reg.try_claim("c", &v(&["docs/book.md"])).unwrap();
        assert!(reg.try_claim("b", &v(&["docs/"])).is_err());
        // Disjoint sibling is fine.
        assert!(reg.try_claim("b", &v(&["tests/parser/"])).is_ok());
    }

    #[test]
    fn release_is_idempotent_and_frees_the_scope() {
        let reg = OwnershipRegistry::new();
        reg.try_claim("a", &v(&["src/x.rs"])).unwrap();
        reg.release_all("a");
        reg.release_all("a");
        assert!(reg.owned_by("a").is_empty());
        assert!(reg.try_claim("b", &v(&["src/x.rs"])).is_ok());
    }

    #[test]
    fn parent_fence_reports_conflicts_for_owned_targets() {
        let reg = OwnershipRegistry::new();
        reg.register_owner("child-1", "Newton (child-1)");
        reg.try_claim("child-1", &v(&["src/output/"])).unwrap();
        let hits = reg.conflicts_for(&v(&["src/output/json.rs"]), "parent");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].owner, "Newton (child-1)");
        assert!(
            reg.conflicts_for(&v(&["src/input.rs"]), "parent")
                .is_empty()
        );
    }

    #[test]
    fn an_active_parent_mutation_denies_the_claim_retryably() {
        let reg = OwnershipRegistry::new();
        {
            let _guard = reg.parent_mutation_guard(v(&["src/main.rs"]));
            let err = reg.try_claim("a", &v(&["src/main.rs"])).unwrap_err();
            assert!(matches!(err, ClaimRejection::Conflicts(_)));
        }
        // Guard dropped: claim now succeeds.
        assert!(reg.try_claim("a", &v(&["src/main.rs"])).is_ok());
    }
}
