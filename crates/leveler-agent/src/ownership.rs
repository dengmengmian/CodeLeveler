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
pub struct OwnershipRegistry {
    state: Mutex<RegistryState>,
    /// The workspace volume's real case semantics, fixed at construction.
    case_insensitive: bool,
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

/// Containment key. Ownership is exclusivity over a FILE, not over a spelling:
/// on a volume that folds case, `src/Parser.rs` and `src/parser.rs` are the
/// same bytes, so treating them as distinct hands two children "exclusive"
/// ownership of one file and every fence downstream passes both writes.
///
/// The answer comes from the workspace's ACTUAL volume
/// (`Workspace::path_case_insensitive`), never from the platform: macOS
/// supports case-sensitive volumes and a Linux root can sit on a
/// case-insensitive mount, so a `cfg!` guess is wrong on real configurations
/// in both directions.
fn fold(path: &str, case_insensitive: bool) -> String {
    if case_insensitive {
        path.to_lowercase()
    } else {
        path.to_string()
    }
}

fn covers(owner: &str, target: &str, case_insensitive: bool) -> bool {
    let (owner, target) = (
        fold(owner, case_insensitive),
        fold(target, case_insensitive),
    );
    owner == target
        || target.starts_with(&format!("{owner}/"))
        || owner.starts_with(&format!("{target}/"))
}

/// Conservative default: fold case. A derived `Default` would answer "this
/// volume is case-sensitive", which is the direction that GRANTS two owners the
/// same file — never let that be the fallback.
impl Default for OwnershipRegistry {
    fn default() -> Self {
        Self::new(true)
    }
}

impl OwnershipRegistry {
    /// For a workspace whose volume folds case (`Foo.rs` == `foo.rs`). Callers
    /// pass `Workspace::path_case_insensitive()`; `true` is the conservative
    /// answer when the volume cannot be probed.
    pub fn new(case_insensitive: bool) -> Self {
        Self {
            state: Mutex::new(RegistryState::default()),
            case_insensitive,
        }
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
            .filter(|p| {
                !already
                    .iter()
                    .any(|own| covers(own, p, self.case_insensitive))
            })
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
                if owned.iter().any(|o| covers(o, p, self.case_insensitive)) {
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
            if state
                .parent_active
                .iter()
                .any(|a| covers(a, p, self.case_insensitive))
            {
                conflicts.push(ClaimConflict {
                    path: p.clone(),
                    // Not "retry shortly": the parent holds this from the
                    // ownership check until its call resolves, which spans an
                    // approval prompt and so can be an unbounded human wait.
                    // Promising a short retry burns the child's rounds.
                    owner: "parent (mutation in flight — claim something else, \
                            or retry after it settles)"
                        .to_string(),
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
    ///
    /// This DENIES on a match, so a target that cannot be proven
    /// in-workspace by string comparison (absolute, or containing `..`) fails
    /// CLOSED: it conflicts with every live claim rather than escaping one.
    pub fn conflicts_for(&self, targets: &[String], exclude_owner: &str) -> Vec<ClaimConflict> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::conflicts_locked(&state, targets, exclude_owner, self.case_insensitive)
    }

    /// The conflict scan itself, so the check and the parent's mutation guard
    /// can share one lock acquisition (see [`Self::try_mutation_guard`]).
    fn conflicts_locked(
        state: &RegistryState,
        targets: &[String],
        exclude_owner: &str,
        case_insensitive: bool,
    ) -> Vec<ClaimConflict> {
        let mut out = Vec::new();
        for raw in targets {
            let t = normalize(raw);
            let unresolvable = crate::authorization::target_is_unresolvable(&t);
            for (owner, owned) in &state.claims {
                if owner == exclude_owner {
                    continue;
                }
                if unresolvable || owned.iter().any(|o| covers(o, &t, case_insensitive)) {
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

    /// Check ownership and commit to mutating in ONE locked operation, for a
    /// non-child writer (`owner_key` "parent" at depth 0).
    ///
    /// Doing this as two calls — ask `conflicts_for`, then take the guard —
    /// leaves a window in which a background child, running on another worker
    /// thread, claims the same path: its claim sees an empty `parent_active`
    /// and is granted, the guard marks without re-checking, and both writers
    /// proceed into one file. Concurrency here is real parallelism, not a
    /// yield point, so "there is no await between the two calls" does not
    /// close it. Callers must not re-implement the two-step form.
    pub fn try_mutation_guard(
        &self,
        owner_key: &str,
        targets: &[String],
    ) -> Result<ParentMutationGuard<'_>, Vec<ClaimConflict>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let conflicts = Self::conflicts_locked(&state, targets, owner_key, self.case_insensitive);
        if !conflicts.is_empty() {
            return Err(conflicts);
        }
        let normalized: Vec<String> = targets.iter().map(|p| normalize(p)).collect();
        state.parent_active.extend(normalized.iter().cloned());
        drop(state);
        Ok(ParentMutationGuard {
            registry: self,
            paths: normalized,
        })
    }
}

impl std::fmt::Debug for ParentMutationGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParentMutationGuard")
            .field("paths", &self.paths)
            .finish()
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
        let reg = OwnershipRegistry::new(true);
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
        let reg = OwnershipRegistry::new(true);
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
        let reg = OwnershipRegistry::new(true);
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
        let reg = OwnershipRegistry::new(true);
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
        let reg = OwnershipRegistry::new(true);
        reg.try_claim("a", &v(&["src/x.rs"])).unwrap();
        reg.release_all("a");
        reg.release_all("a");
        assert!(reg.owned_by("a").is_empty());
        assert!(reg.try_claim("b", &v(&["src/x.rs"])).is_ok());
    }

    #[test]
    fn an_unresolvable_target_fails_closed_against_every_live_claim() {
        // The fence denies on a match, so `foo/../b.txt` or an absolute path
        // must not be provable-outside by string comparison (regression of
        // the old fence's fail-open hole, carried over with the rewrite).
        let reg = OwnershipRegistry::new(true);
        reg.register_owner("child-1", "Euclid (child-1)");
        reg.try_claim("child-1", &v(&["src/owned.rs"])).unwrap();
        for sneaky in ["foo/../b.txt", "/etc/passwd", "../escape.rs"] {
            assert!(
                !reg.conflicts_for(&v(&[sneaky]), "parent").is_empty(),
                "{sneaky} must fail closed while any claim is live"
            );
        }
        // With no live claim there is nothing to conflict with.
        reg.release_all("child-1");
        assert!(
            reg.conflicts_for(&v(&["foo/../b.txt"]), "parent")
                .is_empty()
        );
    }

    #[test]
    fn parent_fence_reports_conflicts_for_owned_targets() {
        let reg = OwnershipRegistry::new(true);
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
        let reg = OwnershipRegistry::new(true);
        {
            let _guard = reg
                .try_mutation_guard("parent", &v(&["src/main.rs"]))
                .unwrap();
            let err = reg.try_claim("a", &v(&["src/main.rs"])).unwrap_err();
            assert!(matches!(err, ClaimRejection::Conflicts(_)));
        }
        // Guard dropped: claim now succeeds.
        assert!(reg.try_claim("a", &v(&["src/main.rs"])).is_ok());
    }

    // ── CI1-CI5: exclusivity follows the FILE, not the spelling ──────────
    //
    // The semantics come from the workspace volume, so both directions are
    // asserted against an explicitly-constructed registry rather than against
    // whatever filesystem the test host happens to run on.

    /// CI1: an identical spelling conflicts on either volume.
    #[test]
    fn an_identical_path_conflicts_under_both_case_semantics() {
        for case_insensitive in [true, false] {
            let reg = OwnershipRegistry::new(case_insensitive);
            reg.try_claim("a", &v(&["src/parser.rs"])).unwrap();
            assert!(
                matches!(
                    reg.try_claim("b", &v(&["src/parser.rs"])),
                    Err(ClaimRejection::Conflicts(_))
                ),
                "case_insensitive={case_insensitive}"
            );
        }
    }

    /// CI3: on a case-sensitive volume the two spellings are genuinely two
    /// files, so denying the second claim is a false denial that costs real
    /// work. This is the half a platform-hardcoded fold gets wrong — a
    /// case-sensitive volume on macOS is a supported configuration.
    #[test]
    fn a_case_alias_is_a_distinct_file_on_a_case_sensitive_volume() {
        let reg = OwnershipRegistry::new(false);
        reg.try_claim("a", &v(&["src/Parser.rs"])).unwrap();
        // a's scope must not reach the other file — checked before anyone
        // else claims it, so the query answers about a's scope alone.
        assert!(
            reg.conflicts_for(&v(&["src/parser.rs"]), "parent")
                .is_empty(),
            "a's scope must not fence a genuinely different file"
        );
        let owned = reg
            .try_claim("b", &v(&["src/parser.rs"]))
            .expect("distinct physical files must both be claimable");
        assert_eq!(owned, v(&["src/parser.rs"]));
    }

    /// CI4: subtree coverage uses the same identity, so a case-aliased
    /// DIRECTORY claim cannot slip a child under someone else's tree.
    #[test]
    fn subtree_coverage_uses_the_same_path_identity() {
        let folding = OwnershipRegistry::new(true);
        folding.try_claim("a", &v(&["src/Output/"])).unwrap();
        assert!(matches!(
            folding.try_claim("b", &v(&["src/output/json.rs"])),
            Err(ClaimRejection::Conflicts(_))
        ));
        let sensitive = OwnershipRegistry::new(false);
        sensitive.try_claim("a", &v(&["src/Output/"])).unwrap();
        assert!(
            sensitive
                .try_claim("b", &v(&["src/output/json.rs"]))
                .is_ok()
        );
    }

    /// CI5: folding must not soften the boundary checks — they run before any
    /// containment math and reject under either semantics.
    #[test]
    fn case_folding_never_permits_escape_or_root_claims() {
        for case_insensitive in [true, false] {
            let reg = OwnershipRegistry::new(case_insensitive);
            for bad in ["..", "../Secret", "/etc/Passwd", ".", "./", ""] {
                assert!(
                    reg.try_claim("a", &v(&[bad])).is_err(),
                    "{bad:?} was claimable with case_insensitive={case_insensitive}"
                );
            }
        }
    }

    /// CI2: on a case-folding volume `src/Parser.rs` and `src/parser.rs` are
    /// the same bytes on disk, so granting both hands two children "exclusive"
    /// ownership of one file and every downstream fence — the parent guard
    /// included — waves both writes through.
    #[test]
    fn a_case_variant_spelling_cannot_claim_an_already_owned_file() {
        let reg = OwnershipRegistry::new(true);
        reg.register_owner("a", "Euclid (a)");
        reg.try_claim("a", &v(&["src/Parser.rs"])).unwrap();
        let err = reg
            .try_claim("b", &v(&["src/parser.rs"]))
            .expect_err("a case-variant of an owned path must not be granted too");
        assert!(matches!(err, ClaimRejection::Conflicts(_)), "{err:?}");
        assert!(
            !reg.conflicts_for(&v(&["src/parser.rs"]), "parent")
                .is_empty(),
            "the parent fence must also see the case-variant as owned"
        );
    }

    /// The race the parent fence had: "is it free?" and "I am mutating it"
    /// were two locked operations, and a background child runs on another
    /// worker thread, so the gap between them is real parallel time rather
    /// than a yield point. A claim landing in that gap was granted (it saw an
    /// empty `parent_active`) and the parent then took its guard without
    /// re-checking, so both wrote the same file.
    ///
    /// Deterministic by construction: the interleaving is performed in the
    /// exact order the drive loop performs it, so this proves the API
    /// contract rather than hoping a stress loop hits the window.
    #[test]
    fn a_claim_landing_after_the_parent_check_still_blocks_the_parent() {
        let reg = OwnershipRegistry::new(true);
        reg.register_owner("child-1", "Euclid (child-1)");
        let targets = v(&["src/main.rs"]);

        // Parent admission, step 1: nobody owns it yet.
        assert!(reg.conflicts_for(&targets, "parent").is_empty());

        // The window: a background child claims the same path.
        reg.try_claim("child-1", &targets).unwrap();

        // Parent admission, step 2. Committing to the mutation must re-check
        // under the same lock, or the parent writes a child-owned file.
        let denied = reg
            .try_mutation_guard("parent", &targets)
            .expect_err("the parent must not acquire a mutation guard on a claimed path");
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].owner, "Euclid (child-1)");
    }

    /// The guard must still be acquirable — and still block overlapping
    /// claims — when the path really is free, or the fix would deadlock all
    /// parent writes.
    #[test]
    fn an_uncontended_parent_mutation_still_takes_the_guard_and_fences_claims() {
        let reg = OwnershipRegistry::new(true);
        let targets = v(&["src/main.rs"]);
        {
            let _guard = reg
                .try_mutation_guard("parent", &targets)
                .expect("an unowned path must be guardable");
            assert!(
                matches!(
                    reg.try_claim("child-1", &targets),
                    Err(ClaimRejection::Conflicts(_))
                ),
                "a claim overlapping an in-flight parent mutation is denied retryably"
            );
        }
        // Guard dropped with the call: the path is claimable again.
        assert!(reg.try_claim("child-1", &targets).is_ok());
    }
}
