//! Pending-candidate pipeline: propose → accept / reject.
//!
//! Accept is the **user-consent** write path (same trust level as CLI
//! `leveler memory remember`). Agent tools must not call [`MemoryStore::accept`]
//! under auto-approve; K36 keeps model-facing `remember` / `forget` /
//! `consolidate_memory` denied by [`leveler_execution::AutoApprove`].

use std::fs;

use serde::{Deserialize, Serialize};

use crate::candidates::{
    CandidateKind, MemoryCandidate, detect_package_manager, parse_explicit_remember_intent,
};
use crate::{
    MemoryEntry, MemoryError, MemoryStore, entry_from_candidate, now_rfc3339, write_atomically_pub,
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
            if let Some(existing) = self.find_active_by_key(key)? {
                return Ok(ProposeOutcome::AlreadyActive { id: existing.id });
            }
        }
        // Same fingerprint already pending?
        for pending in self.list_pending()? {
            if pending.fingerprint == candidate.fingerprint {
                return Ok(ProposeOutcome::AlreadyPending(pending));
            }
            if candidate.key.is_some() && pending.key == candidate.key {
                return Ok(ProposeOutcome::AlreadyPending(pending));
            }
        }

        let path = self.pending_path(&candidate.id);
        // Avoid clobbering a different pending id collision: suffix if needed.
        let mut candidate = candidate;
        if path.exists() {
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
        Ok(ProposeOutcome::Pending(candidate))
    }

    /// Parse user text for explicit remember intent and propose if any.
    pub fn propose_from_user_text(
        &self,
        text: &str,
    ) -> Result<Option<ProposeOutcome>, MemoryError> {
        let Some(c) = parse_explicit_remember_intent(text) else {
            return Ok(None);
        };
        Ok(Some(self.propose(c)?))
    }

    /// Detect package manager at `repo_root` and propose at most one candidate.
    pub fn propose_package_manager(
        &self,
        repo_root: &std::path::Path,
    ) -> Result<Option<ProposeOutcome>, MemoryError> {
        let Some(c) = detect_package_manager(repo_root) else {
            return Ok(None);
        };
        Ok(Some(self.propose(c)?))
    }

    pub fn list_pending(&self) -> Result<Vec<MemoryCandidate>, MemoryError> {
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
        let candidate = self.read_pending(id)?;
        let entry = entry_from_candidate(&candidate);
        let saved = if let Some(key) = &entry.key {
            // Upsert by structured key: replace existing active with same key.
            if let Some(old) = self.find_active_by_key(key)? {
                let _ = self.forget(&old.id);
            }
            self.remember(entry)?
        } else {
            self.remember_deduplicated(entry)?
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
            .find(|e| e.key.as_deref() == Some(key)))
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
    let mut out = Vec::new();
    if let Some(o) = store.propose_from_user_text(user_text)? {
        out.push(o);
    }
    if let Some(root) = repo_root
        && let Some(o) = store.propose_package_manager(root)?
    {
        out.push(o);
    }
    Ok(out)
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use crate::candidates::{
        CandidateSource, detect_package_manager, parse_explicit_remember_intent,
    };
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn accept_explicit_intent_then_search_and_index_hit() {
        let dir = tempdir().unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        // Explicit-intent extractor is the public entry; then accept → recall/index.
        let extracted = parse_explicit_remember_intent("记住：用 pnpm").expect("intent");
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
        let repo = tempdir().unwrap();
        fs::write(repo.path().join("pnpm-lock.yaml"), "lockfileVersion: '9'\n").unwrap();

        let store = MemoryStore::open(dir.path()).unwrap();
        let outcome = store
            .propose_package_manager(repo.path())
            .unwrap()
            .expect("pm candidate");
        let pending = match outcome {
            ProposeOutcome::Pending(c) => c,
            other => panic!("expected pending: {other:?}"),
        };
        store.reject(&pending.id).unwrap();
        assert_eq!(store.list_active().unwrap().len(), 0);
        assert_eq!(store.list_pending().unwrap().len(), 0);

        let again = store.propose_package_manager(repo.path()).unwrap();
        match again {
            Some(ProposeOutcome::Suppressed { .. }) => {}
            Some(ProposeOutcome::Pending(_)) => panic!("must not re-spam after reject"),
            other => panic!("unexpected {other:?}"),
        }
        // collect_turn_candidates also respects key suppress.
        let batch = collect_turn_candidates(&store, "hello", Some(repo.path())).unwrap();
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
            .propose_from_user_text("remember: always use gated writes")
            .unwrap();
        assert_eq!(
            store.counts().unwrap(),
            (0, 0),
            "propose must not create active entries"
        );
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }

    #[test]
    fn package_manager_at_most_one_pending() {
        let dir = tempdir().unwrap();
        let repo = tempdir().unwrap();
        fs::write(repo.path().join("pnpm-lock.yaml"), "").unwrap();
        let store = MemoryStore::open(dir.path()).unwrap();
        let first = store.propose_package_manager(repo.path()).unwrap();
        assert!(matches!(first, Some(ProposeOutcome::Pending(_))));
        let second = store.propose_package_manager(repo.path()).unwrap();
        assert!(
            matches!(
                second,
                Some(ProposeOutcome::AlreadyPending(_)) | Some(ProposeOutcome::Pending(_))
            ) || matches!(second, Some(ProposeOutcome::AlreadyPending(_))),
            "{second:?}"
        );
        // Still only one pending file for the package_manager key.
        let pending = store.list_pending().unwrap();
        let pm: Vec<_> = pending
            .iter()
            .filter(|c| c.key.as_deref() == Some("package_manager"))
            .collect();
        assert_eq!(pm.len(), 1, "{pending:?}");
    }

    #[test]
    fn extractors_drive_real_pipeline_entry_points() {
        // Structural: shipped public functions are what CLI/app should call.
        let c = parse_explicit_remember_intent("记住：用 pnpm").expect("intent");
        assert_eq!(c.source, CandidateSource::UserExplicit);
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();
        let pm = detect_package_manager(dir.path()).expect("yarn");
        assert!(pm.body.contains("yarn"));
    }

    #[test]
    fn secret_body_never_accepted_via_extractor() {
        assert!(
            parse_explicit_remember_intent("记住：api_key=sk-live-secret-value-here").is_none()
        );
    }
}
