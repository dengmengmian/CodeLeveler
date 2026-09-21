//! Pending-candidate pipeline: propose → accept / reject.
//!
//! Accept is the **user-consent** write path (same trust level as CLI
//! `leveler memory remember`). Agent tools must not call [`MemoryStore::accept`]
//! under auto-approve; K36 keeps model-facing `remember` / `forget` denied by
//! [`leveler_execution::AutoApprove`].

use std::fs;

use serde::{Deserialize, Serialize};

use crate::candidates::{CandidateKind, MemoryCandidate, parse_inferred_preference};
use crate::{
    CommitOutcome, MemoryAuthority, MemoryEntry, MemoryError, MemoryStore, now_rfc3339,
    write_atomically_pub,
};

/// Result of attempting to enqueue a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposeOutcome {
    /// Written under `pending/`.
    Pending(MemoryCandidate),
    /// Same fingerprint was previously rejected — not re-queued.
    Suppressed { fingerprint: String },
    /// An active entry with the same structured key already exists.
    AlreadyActive { id: String },
    /// Identical pending fingerprint already queued.
    AlreadyPending(MemoryCandidate),
}

/// Runtime admission keeps new topics behind consent while allowing a user's
/// explicit correction of an already-known fact to take effect immediately.
#[derive(Debug, Clone)]
pub enum AdmitOutcome {
    Proposed(ProposeOutcome),
    Corrected(CommitOutcome),
}

/// Recorded when the user rejects a candidate (suppress re-prompt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuppressRecord {
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub rejected_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_id: Option<String>,
}

impl MemoryStore {
    /// Admit a validated runtime candidate.
    ///
    /// A new identity is only proposed. A different value for an existing
    /// identity may update immediately only when the candidate is backed by
    /// explicit user authority; weaker observations still require consent.
    pub fn admit_candidate(&self, candidate: MemoryCandidate) -> Result<AdmitOutcome, MemoryError> {
        if candidate.authority == MemoryAuthority::ExplicitUser
            && candidate.key.is_some()
            && let Some(existing) = self.find_active_for_candidate(&candidate)?
            && existing.body.trim() != candidate.body.trim()
        {
            let outcome = self.commit_candidate(&candidate)?;
            // Heal an equivalent proposal that an interactive admission path
            // may have queued before the executor observed the correction.
            let _ = self.list_pending();
            return Ok(AdmitOutcome::Corrected(outcome));
        }
        self.propose(candidate).map(AdmitOutcome::Proposed)
    }

    /// Enqueue a candidate for user consent. Never writes `active/`.
    pub fn propose(&self, candidate: MemoryCandidate) -> Result<ProposeOutcome, MemoryError> {
        if self.is_suppressed(&candidate.fingerprint)? {
            return Ok(ProposeOutcome::Suppressed {
                fingerprint: candidate.fingerprint.clone(),
            });
        }
        if let Some(key) = &candidate.key {
            if self.is_key_suppressed(candidate.kind, key)? {
                return Ok(ProposeOutcome::Suppressed {
                    fingerprint: candidate.fingerprint.clone(),
                });
            }
            if let Some(existing) = self.find_active_by_key(key)?
                && existing.body.trim() == candidate.body.trim()
            {
                return Ok(ProposeOutcome::AlreadyActive { id: existing.id });
            }
            // Same identity, different value: this is an update proposal.
            // It must wait for consent, but must not be discarded merely
            // because an older value is currently active.
        }
        // Same fingerprint already pending?
        let mut replaces_pending = None;
        for pending in self.list_pending()? {
            if pending.fingerprint == candidate.fingerprint {
                return Ok(ProposeOutcome::AlreadyPending(pending));
            }
            if candidate.key.is_some() && pending.key == candidate.key {
                if pending.body.trim() == candidate.body.trim() {
                    return Ok(ProposeOutcome::AlreadyPending(pending));
                }
                // A later correction for the same identity replaces the stale
                // unapproved proposal. Pending state is not history; keeping
                // both would let the user accidentally accept the old value.
                replaces_pending = Some(pending.id);
                break;
            }
        }

        let path = self.pending_path(&candidate.id);
        // Avoid clobbering a different pending id collision: suffix if needed.
        let mut candidate = candidate;
        let replaces_same_path = replaces_pending
            .as_deref()
            .is_some_and(|id| self.pending_path(id) == path);
        if path.exists() && !replaces_same_path {
            let mut n = 2u32;
            loop {
                let alt_id = format!("{}-{n}", candidate.id);
                let alt = self.pending_path(&alt_id);
                if !alt.exists() {
                    candidate.id = alt_id;
                    break;
                }
                n = n
                    .checked_add(1)
                    .ok_or_else(|| MemoryError::Invalid("too many pending id collisions".into()))?;
            }
        }
        let json = serde_json::to_string_pretty(&candidate)?;
        write_atomically_pub(&self.pending_path(&candidate.id), json.as_bytes())?;
        if let Some(old_id) = replaces_pending
            && old_id != candidate.id
        {
            let _ = fs::remove_file(self.pending_path(&old_id));
        }
        Ok(ProposeOutcome::Pending(candidate))
    }

    /// Parse user text for explicit remember intent and propose if any.
    pub fn propose_from_user_text(
        &self,
        text: &str,
    ) -> Result<Option<ProposeOutcome>, MemoryError> {
        let Some(c) = parse_inferred_preference(text) else {
            return Ok(None);
        };
        Ok(Some(self.propose(c)?))
    }

    /// Candidates awaiting consent.
    ///
    /// Heals crash residue first: a candidate whose content an active entry
    /// already covers is removed, so a crash between activating and clearing
    /// cannot leave the user approving something already stored.
    pub fn list_pending(&self) -> Result<Vec<MemoryCandidate>, MemoryError> {
        let covered: Vec<String> = self
            .list_active()?
            .into_iter()
            .map(|e| e.body.trim().to_string())
            .collect();
        for candidate in self.list_pending_raw()? {
            if covered.iter().any(|b| b == candidate.body.trim()) {
                let _ = fs::remove_file(self.pending_path(&candidate.id));
            }
        }
        self.list_pending_raw()
    }

    /// Every pending candidate on disk, with no reconciliation. The internal
    /// read [`Self::list_pending`] and activation build on.
    pub(crate) fn list_pending_raw(&self) -> Result<Vec<MemoryCandidate>, MemoryError> {
        let dir = self.root.join("pending");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = fs::read_to_string(path)?;
            out.push(serde_json::from_str(&raw)?);
        }
        out.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(out)
    }

    pub fn read_pending(&self, id: &str) -> Result<MemoryCandidate, MemoryError> {
        let path = self.pending_path(id);
        if !path.exists() {
            return Err(MemoryError::NotFound(id.to_string()));
        }
        let raw = fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// User consent: promote pending → active durable memory.
    ///
    /// This is the explicit accept path (CLI / UI). It is **not** safe to call
    /// from auto-approved agent loops without a separate human decision.
    pub fn accept(&self, id: &str) -> Result<MemoryEntry, MemoryError> {
        let mut candidate = self.read_pending(id)?;
        // Accepting is the user's authority. Route it through the same
        // lifecycle transaction as runtime corrections so a changed semantic
        // identity preserves the old value as superseded history instead of
        // reusing its id and deleting that history.
        candidate.authority = crate::lifecycle::MemoryAuthority::ExplicitUser;
        candidate.source = crate::candidates::CandidateSource::UserExplicit;
        let outcome = self.commit_candidate(&candidate)?;
        let saved = match outcome.entry {
            Some(entry) => entry,
            None => {
                // An identical candidate is an idempotent accept. Return the
                // current truth rather than manufacturing another entry.
                if let Some(key) = candidate.key.as_deref()
                    && let Some(active) = self.find_active_by_key(key)?
                {
                    active
                } else if let Some(active) = self
                    .list_active()?
                    .into_iter()
                    .find(|entry| entry.body.trim() == candidate.body.trim())
                {
                    active
                } else {
                    return Err(MemoryError::Invalid(outcome.skipped_reason.unwrap_or_else(
                        || "accepted candidate produced no active memory".into(),
                    )));
                }
            }
        };
        let _ = fs::remove_file(self.pending_path(id));
        // Accepting clears suppress for this fingerprint so a later genuine
        // update can be proposed again after forget.
        let _ = fs::remove_file(self.suppress_path(&candidate.fingerprint));
        Ok(saved)
    }

    /// Reject pending candidate and suppress the same signal from re-proposing.
    pub fn reject(&self, id: &str) -> Result<SuppressRecord, MemoryError> {
        let candidate = self.read_pending(id)?;
        let record = SuppressRecord {
            fingerprint: candidate.fingerprint.clone(),
            key: candidate.key.clone(),
            rejected_at: now_rfc3339(),
            candidate_id: Some(candidate.id.clone()),
        };
        let json = serde_json::to_string_pretty(&record)?;
        write_atomically_pub(&self.suppress_path(&record.fingerprint), json.as_bytes())?;
        // Also suppress by key alone for package_manager so any wording of the
        // same signal stays quiet.
        if let Some(key) = &candidate.key {
            let key_fp =
                crate::candidates::fingerprint_of(candidate.kind, Some(key.as_str()), "", "");
            let key_record = SuppressRecord {
                fingerprint: key_fp.clone(),
                key: Some(key.clone()),
                rejected_at: record.rejected_at.clone(),
                candidate_id: Some(candidate.id.clone()),
            };
            let kj = serde_json::to_string_pretty(&key_record)?;
            write_atomically_pub(&self.suppress_path(&key_fp), kj.as_bytes())?;
        }
        let _ = fs::remove_file(self.pending_path(id));
        Ok(record)
    }

    pub fn is_suppressed(&self, fingerprint: &str) -> Result<bool, MemoryError> {
        Ok(self.suppress_path(fingerprint).exists())
    }

    /// Whether a structured key (e.g. package_manager) is suppressed.
    pub fn is_key_suppressed(&self, kind: CandidateKind, key: &str) -> Result<bool, MemoryError> {
        let fp = crate::candidates::fingerprint_of(kind, Some(key), "", "");
        self.is_suppressed(&fp)
    }

    pub fn find_active_by_key(&self, key: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        Ok(self
            .list_active()?
            .into_iter()
            .find(|entry| crate::lifecycle::durable_entry_key(entry).as_deref() == Some(key)))
    }

    fn find_active_for_candidate(
        &self,
        candidate: &MemoryCandidate,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        Ok(self
            .list_active()?
            .into_iter()
            .find(|entry| crate::lifecycle::entry_matches_candidate(entry, candidate)))
    }

    /// Once an explicit write establishes the current truth, every older
    /// unapproved proposal for that identity is stale. Removing all of them
    /// prevents a later accept from reverting the correction.
    pub(crate) fn clear_pending_for_key(&self, key: &str) -> Result<(), MemoryError> {
        for candidate in self.list_pending_raw()? {
            let candidate_key = candidate
                .semantic_key
                .as_deref()
                .or(candidate.key.as_deref());
            if candidate_key == Some(key) {
                match fs::remove_file(self.pending_path(&candidate.id)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(MemoryError::Io(error)),
                }
            }
        }
        Ok(())
    }
}

/// Convenience: extract + propose from user text and optional repo root (PM).
///
/// Returns all propose outcomes (explicit intent first, then package manager).
/// Never writes active memory.
pub fn collect_turn_candidates(
    store: &MemoryStore,
    user_text: &str,
    repo_root: Option<&std::path::Path>,
) -> Result<Vec<ProposeOutcome>, MemoryError> {
    // Repository-derived facts are deliberately NOT proposed here. A lockfile
    // or `packageManager` field is readable from the working tree on demand, so
    // storing a copy only creates a second source of truth that goes stale
    // (a project that moved pnpm -> bun would keep being told `pnpm`).
    // `candidates::package_manager_from_root` remains for callers that need the
    // fact — they read it fresh instead of remembering it.
    let _ = repo_root;
    let mut out = Vec::new();
    if let Some(candidate) = crate::parse_durable_fact(user_text) {
        out.push(store.propose(candidate)?);
    } else if let Some(o) = store.propose_from_user_text(user_text)? {
        out.push(o);
    }
    Ok(out)
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use crate::candidates::{CandidateSource, parse_inferred_preference};
    use std::fs;
    use tempfile::tempdir;

    /// A lockfile is a fact about the working tree, not a decision a person
    /// made. Copying it into durable memory creates a second source of truth
    /// that goes stale the moment the project switches package managers, so a
    /// normal turn must propose nothing for it.
    #[test]
    fn a_lockfile_proposes_no_memory() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let repo = tempdir().unwrap();
        fs::write(repo.path().join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();

        let outcomes =
            collect_turn_candidates(&store, "帮我看一下这个组件", Some(repo.path())).unwrap();
        assert!(
            outcomes.is_empty(),
            "an ordinary turn proposes nothing from a lockfile: {outcomes:?}"
        );
        assert_eq!(store.list_pending().unwrap().len(), 0);
        assert_eq!(store.list_active().unwrap().len(), 0);

        // The detector itself is kept — other features may need to know the
        // package manager, they just read it from the repository each time.
        assert_eq!(
            crate::candidates::package_manager_from_root(repo.path()),
            Some("pnpm")
        );
    }

    /// Explicit user intent is still collected in the same turn.
    #[test]
    fn user_intent_is_still_collected_alongside_a_lockfile() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let repo = tempdir().unwrap();
        fs::write(repo.path().join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();

        let outcomes =
            collect_turn_candidates(&store, "我通常希望提交前先跑 lint", Some(repo.path()))
                .unwrap();
        assert_eq!(
            outcomes.len(),
            1,
            "only the user's own intent: {outcomes:?}"
        );
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }

    #[test]
    fn a_strong_durable_fact_is_not_duplicated_by_the_soft_fallback() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();

        let outcomes = collect_turn_candidates(&store, "以后最好默认使用 pnpm", None).unwrap();

        assert_eq!(outcomes.len(), 1);
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }

    #[test]
    fn accept_explicit_intent_then_search_and_index_hit() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        // Explicit-intent extractor is the public entry; then accept → recall/index.
        let extracted = parse_inferred_preference("我通常希望用 pnpm").expect("intent");
        let outcome = store.propose(extracted).unwrap();
        let pending = match outcome {
            ProposeOutcome::Pending(c) => c,
            other => panic!("expected pending, got {other:?}"),
        };
        // Replace pending with a short-title / long-body candidate so INDEX
        // (titles only) is observably body-free — still the real accept path.
        let long_body = "本仓库安装与脚本一律用 pnpm；日常启动用 pnpm run dev，\
                         不要默认 npm；私密令牌 never-in-index-token-xyz 不得进 INDEX。";
        let refined = MemoryCandidate::new(
            "包管理器偏好",
            long_body,
            CandidateKind::Preference,
            None,
            CandidateSource::UserExplicit,
            vec!["preference".into()],
        )
        .unwrap();
        let _ = fs::remove_file(store.pending_path(&pending.id));
        let pending = match store.propose(refined).unwrap() {
            ProposeOutcome::Pending(c) => c,
            other => panic!("expected pending refined: {other:?}"),
        };
        assert_eq!(store.list_active().unwrap().len(), 0);
        assert_eq!(store.list_pending().unwrap().len(), 1);

        let saved = store.accept(&pending.id).unwrap();
        assert_eq!(store.list_pending().unwrap().len(), 0);
        assert_eq!(store.list_active().unwrap().len(), 1);

        let hits = store.search("pnpm", 5).unwrap();
        assert!(
            !hits.is_empty() && hits[0].0.body.contains("pnpm"),
            "recall must surface accepted body: {hits:?}"
        );
        let index = store.index_lines(10).unwrap();
        assert!(index.contains(&saved.id), "index missing id: {index}");
        assert!(
            index.contains("包管理器偏好"),
            "index should list title: {index}"
        );
        assert!(
            !index.contains("never-in-index-token-xyz"),
            "index must not leak body token: {index}"
        );
        assert!(!index.contains(saved.body.as_str()));
    }

    #[test]
    fn reject_leaves_active_empty_and_suppresses_repropose() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        // Any candidate exercises reject/suppress; a user preference is used
        // because repository-derived facts are no longer proposed at all.
        let text = "我通常希望提交前先跑 lint";
        let outcome = store
            .propose_from_user_text(text)
            .unwrap()
            .expect("candidate");
        let pending = match outcome {
            ProposeOutcome::Pending(c) => c,
            other => panic!("expected pending: {other:?}"),
        };
        store.reject(&pending.id).unwrap();
        assert_eq!(store.list_active().unwrap().len(), 0);
        assert_eq!(store.list_pending().unwrap().len(), 0);

        match store.propose_from_user_text(text).unwrap() {
            Some(ProposeOutcome::Suppressed { .. }) => {}
            Some(ProposeOutcome::Pending(_)) => panic!("must not re-spam after reject"),
            other => panic!("unexpected {other:?}"),
        }
        // collect_turn_candidates respects the same suppression.
        let batch = collect_turn_candidates(&store, text, None).unwrap();
        assert!(
            batch
                .iter()
                .all(|o| !matches!(o, ProposeOutcome::Pending(_))),
            "no new pending after suppress: {batch:?}"
        );
    }

    #[test]
    fn propose_never_writes_active_without_accept() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let _ = store
            .propose_from_user_text("我通常希望用 gated writes")
            .unwrap();
        assert_eq!(
            store.counts().unwrap(),
            (0, 0),
            "propose must not create active entries"
        );
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }

    #[test]
    fn changed_value_for_active_key_waits_then_supersedes_on_accept() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let old = crate::parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        let old_id = store.commit_candidate(&old).unwrap().entry.unwrap().id;
        let changed = crate::parse_durable_fact("以后本项目默认模型改为 Flash").unwrap();

        let pending = match store.propose(changed).unwrap() {
            ProposeOutcome::Pending(candidate) => candidate,
            other => panic!("changed value must wait for consent: {other:?}"),
        };
        assert_eq!(store.effective_active().unwrap().len(), 1);
        assert!(store.effective_active().unwrap()[0].body.contains("Pro"));

        let saved = store.accept(&pending.id).unwrap();
        assert!(saved.body.contains("Flash"));
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Flash"));
        let history = store.list_archived().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, old_id);
        assert!(
            store
                .search("Pro", 10)
                .unwrap()
                .into_iter()
                .all(|(entry, _)| entry.id != old_id),
            "superseded history must not be recalled"
        );
    }

    #[test]
    fn explicit_runtime_correction_supersedes_without_a_second_candidate() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let old = crate::parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        let old_id = store.commit_candidate(&old).unwrap().entry.unwrap().id;
        let changed = crate::parse_durable_fact("以后本项目默认模型改为 Flash").unwrap();

        let outcome = match store.admit_candidate(changed).unwrap() {
            AdmitOutcome::Corrected(outcome) => outcome,
            AdmitOutcome::Proposed(other) => panic!("correction stayed pending: {other:?}"),
        };
        assert_eq!(outcome.operation, crate::AppliedOperation::Superseded);
        assert!(store.list_pending().unwrap().is_empty());
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Flash"));
        assert_eq!(store.list_archived().unwrap()[0].id, old_id);
        assert!(
            store
                .search("Pro", 10)
                .unwrap()
                .into_iter()
                .all(|(entry, _)| entry.id != old_id)
        );
    }

    #[test]
    fn explicit_correction_removes_every_stale_pending_value_for_the_key() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let old = crate::parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        store.commit_candidate(&old).unwrap();
        let pending = crate::parse_durable_fact("以后本项目默认模型改为 Flash").unwrap();
        assert!(matches!(
            store.propose(pending).unwrap(),
            ProposeOutcome::Pending(_)
        ));

        let newest = crate::parse_durable_fact("以后本项目默认模型改为 Ultra").unwrap();
        assert!(matches!(
            store.admit_candidate(newest).unwrap(),
            AdmitOutcome::Corrected(_)
        ));

        assert!(store.list_pending().unwrap().is_empty());
        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Ultra"));
    }

    #[test]
    fn a_legacy_keyless_direct_write_is_found_and_superseded() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let old = store
            .activate(
                "旧默认模型",
                "以后本项目默认模型固定为 Pro",
                crate::MemoryKind::Preference,
                Vec::new(),
            )
            .unwrap();
        assert!(
            old.key.is_none(),
            "fixture represents a legacy direct write"
        );

        let changed = crate::parse_durable_fact("以后本项目默认模型改为 Flash").unwrap();
        assert!(matches!(
            store.admit_candidate(changed).unwrap(),
            AdmitOutcome::Corrected(_)
        ));

        let active = store.effective_active().unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].body.contains("Flash"));
        assert_eq!(store.list_archived().unwrap()[0].id, old.id);
    }

    #[test]
    fn a_subject_mention_in_a_note_or_broader_preference_is_not_superseded() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        store
            .activate(
                "后台启动排查",
                "后台启动服务故障排查文档在 docs/runbook.md",
                crate::MemoryKind::Note,
                Vec::new(),
            )
            .unwrap();
        store
            .activate(
                "日志保留",
                "以后后台启动服务的日志保留 7 天",
                crate::MemoryKind::Preference,
                Vec::new(),
            )
            .unwrap();
        let semantic = crate::SemanticCandidate {
            fact: "以后不要后台启动服务".into(),
            subject: "service.background_start".into(),
            value: Some("disabled".into()),
            scope: crate::CandidateScope::Project,
            durability: crate::CandidateDurability::Durable,
            authority: crate::MemoryAuthority::ExplicitUser,
            operation_hint: crate::OperationHint::Update,
            evidence_span: "以后不要后台启动服务了".into(),
            confidence: Some(1.0),
        };
        let candidate =
            crate::validate_semantic_candidate(&semantic, "以后不要后台启动服务了").unwrap();

        assert!(matches!(
            store.admit_candidate(candidate).unwrap(),
            AdmitOutcome::Proposed(ProposeOutcome::Pending(_))
        ));
        assert_eq!(store.effective_active().unwrap().len(), 2);
        assert!(store.list_archived().unwrap().is_empty());
    }

    #[test]
    fn unchanged_value_for_active_key_is_already_active() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let fact = crate::parse_durable_fact("以后本项目默认模型固定为 Pro").unwrap();
        let active_id = store.commit_candidate(&fact).unwrap().entry.unwrap().id;

        assert_eq!(
            store.propose(fact).unwrap(),
            ProposeOutcome::AlreadyActive { id: active_id }
        );
        assert!(store.list_pending().unwrap().is_empty());
    }

    #[test]
    fn extractors_drive_real_pipeline_entry_points() {
        // Structural: shipped public functions are what CLI/app should call.
        let c = parse_inferred_preference("我通常希望用 pnpm").expect("intent");
        assert_eq!(c.source, CandidateSource::SystemInferred);
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();
        // The package manager is READ from the repository, never remembered.
        assert_eq!(
            crate::candidates::package_manager_from_root(dir.path()),
            Some("yarn")
        );
    }

    #[test]
    fn secret_body_never_accepted_via_extractor() {
        assert!(parse_inferred_preference("记住：api_key=sk-live-secret-value-here").is_none());
    }
}
