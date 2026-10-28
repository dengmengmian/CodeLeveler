//! Unified memory service.
//!
//! Every production read and write goes through this facade, and the Markdown
//! store is the only implementation behind it. Authority is derived from the
//! [`Actor`] path: no model payload, CLI argument, or Markdown field can
//! construct an [`Actor::User`]. Tools, the consolidator, the interactive
//! command handlers and the CLI therefore cannot drift into separate write
//! paths.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::v2;
use crate::v2::Actor;
use crate::{CandidateOperation, ExperienceRecord, MemoryCandidate, MemoryLanes};

/// The Markdown scope roots the service was constructed with. A scope is
/// optional so a project run that never touches global memory never creates
/// the user's home store.
#[derive(Debug, Clone, Default)]
pub struct MemoryRoots {
    pub project: Option<v2::ScopeRoots>,
    pub local: Option<v2::ScopeRoots>,
    pub global: Option<v2::ScopeRoots>,
}

impl MemoryRoots {
    /// Roots with no scope configured yet.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_project(mut self, roots: v2::ScopeRoots) -> Self {
        self.project = Some(roots);
        self
    }

    pub fn with_local(mut self, roots: v2::ScopeRoots) -> Self {
        self.local = Some(roots);
        self
    }

    pub fn with_global(mut self, roots: v2::ScopeRoots) -> Self {
        self.global = Some(roots);
        self
    }

    fn scope_roots(&self, scope: ServiceScope) -> Option<&v2::ScopeRoots> {
        match scope {
            ServiceScope::Project => self.project.as_ref(),
            ServiceScope::Local => self.local.as_ref(),
            ServiceScope::Global => self.global.as_ref(),
        }
    }
}

/// The scope a caller addresses. A scope the session did not configure is
/// reported as unavailable instead of being silently mapped onto another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceScope {
    Project,
    Local,
    Global,
}

impl ServiceScope {
    pub fn to_v2(self) -> v2::MemoryScope {
        match self {
            Self::Project => v2::MemoryScope::Project,
            Self::Local => v2::MemoryScope::Local,
            Self::Global => v2::MemoryScope::Global,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Local => "local",
            Self::Global => "global",
        }
    }
}

/// The backend refused or could not perform the request.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("memory store is not configured for this session")]
    NotConfigured,
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Invalid(String),
    #[error("memory not found: {0}")]
    NotFound(String),
    #[error("memory is not current truth: {0}")]
    NotCurrent(String),
    #[error("memory conflict: {0}")]
    Conflict(String),
    #[error("memory requires user review: {0}")]
    NeedsReview(String),
    #[error("{0}")]
    Backend(String),
}

impl From<v2::StoreError> for ServiceError {
    fn from(error: v2::StoreError) -> Self {
        match error {
            v2::StoreError::NotFound(id) => Self::NotFound(id.to_string()),
            v2::StoreError::Forgotten(id) => Self::NotCurrent(id.to_string()),
            v2::StoreError::NotCurrent { id, .. } => Self::NotCurrent(id.to_string()),
            v2::StoreError::Conflict {
                target,
                current,
                expected,
            } => Self::Conflict(format!(
                "{target}: current revision {}, expected {}",
                current.revision, expected.revision
            )),
            v2::StoreError::ForbiddenActor(reason) => Self::NeedsReview(reason),
            v2::StoreError::Busy => Self::Unavailable("memory lock is busy".into()),
            other => Self::Backend(other.to_string()),
        }
    }
}

/// One memory as the service exposes it to a caller. Bodies are present only
/// when the caller asked for a record, not for a list.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryView {
    pub id: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    /// `user_fact` or `experience`.
    pub kind: String,
    /// `active`, `pending`, `superseded`, `deprecated`, `expired`, `archived`.
    pub status: String,
    pub value: Option<String>,
    pub subject_key: Option<String>,
    pub authority: String,
    pub revision: Option<u64>,
    pub current: bool,
    pub expires_at: Option<String>,
    /// RFC 3339 update time from the content authority.
    pub updated_at: Option<String>,
    pub pinned_by_user: bool,
    /// Looks like a credential. Automatic paths and model reads withhold it;
    /// the user's own listing keeps the row so it can be found and archived.
    pub sensitive: bool,
    pub experience: Option<ExperienceView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperienceView {
    pub standing: String,
    pub applies_when: String,
    pub invalid_if: Option<String>,
    pub evidence: Vec<(String, String)>,
}

impl MemoryView {
    pub fn is_experience(&self) -> bool {
        self.kind == "experience"
    }
}

/// A structured identity the extractor may reuse instead of inventing an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityContextEntry {
    pub id: String,
    pub title: String,
    pub subject_key: Option<String>,
    pub subject: Option<String>,
    pub value: Option<String>,
    pub revision: u64,
    pub kind: String,
    pub authority: String,
}

/// What a write did.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteOutcome {
    Applied {
        id: String,
        title: String,
        operation: String,
        revision: Option<u64>,
        superseded: Vec<String>,
        replayed: bool,
        possible_duplicate: Vec<String>,
    },
    Pending {
        id: String,
        title: String,
        reason: String,
    },
    Skipped {
        existing: String,
        reason: String,
    },
    Conflict {
        target: String,
        current_revision: u64,
        expected_revision: u64,
    },
    Rejected {
        reason: String,
    },
    NotStored {
        reason: String,
    },
    Forgotten {
        id: String,
        title: String,
    },
}

impl WriteOutcome {
    pub fn applied_id(&self) -> Option<&str> {
        match self {
            Self::Applied { id, .. } => Some(id),
            _ => None,
        }
    }
}

/// One create/update request. `op_id` is the idempotency identity of the
/// operation, not the fact identity.
#[derive(Debug, Clone)]
pub struct RememberRequest {
    pub op_id: String,
    pub scope: ServiceScope,
    pub title: String,
    pub body: String,
    /// The declared kind. Only `preference`, `decision`, `note` and
    /// `user_fact` are accepted; experience writes carry a structured record.
    pub kind: String,
    pub subject: Option<String>,
    pub value: Option<String>,
    pub tags: Vec<String>,
    pub evidence: Option<String>,
    pub expires_at: Option<String>,
    /// An existing record this write replaces, when the caller named one.
    pub replaces_id: Option<String>,
}

impl RememberRequest {
    pub fn new(
        op_id: impl Into<String>,
        scope: ServiceScope,
        title: impl Into<String>,
        body: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            op_id: op_id.into(),
            scope,
            title: title.into(),
            body: body.into(),
            kind: kind.into(),
            subject: None,
            value: None,
            tags: Vec::new(),
            evidence: None,
            expires_at: None,
            replaces_id: None,
        }
    }
}

/// The unified memory entry point.
pub struct MemoryService {
    roots: MemoryRoots,
    indexes: Mutex<HashMap<ServiceScope, Arc<v2::MemoryIndex>>>,
    stores: Mutex<HashMap<ServiceScope, Arc<v2::MemoryService>>>,
}

impl Clone for MemoryService {
    /// A clone shares roots but not the per-process index handles; opening the
    /// same SQLite file twice is supported by WAL and avoids sharing a mutex
    /// across unrelated callers.
    fn clone(&self) -> Self {
        Self::new(self.roots.clone())
    }
}

impl std::fmt::Debug for MemoryService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryService")
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}

impl MemoryService {
    pub fn new(roots: MemoryRoots) -> Self {
        Self {
            roots,
            indexes: Mutex::new(HashMap::new()),
            stores: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.roots.project.is_some() || self.roots.local.is_some() || self.roots.global.is_some()
    }

    /// The scopes this service can address, in review-surface order.
    ///
    /// A listing covers exactly these stores, in review-surface order.
    pub fn configured_scopes(&self) -> Vec<ServiceScope> {
        [
            ServiceScope::Global,
            ServiceScope::Project,
            ServiceScope::Local,
        ]
        .into_iter()
        .filter(|scope| self.roots.scope_roots(*scope).is_some())
        .collect()
    }

    /// How much store lifecycle work this process has done so far.
    ///
    /// Diagnostics only: the counters describe mechanical work, not memory
    /// content, and nothing in retrieval reads them.
    pub fn lifecycle_metrics(&self) -> v2::StoreLifecycleSnapshot {
        v2::lifecycle_metrics().snapshot()
    }

    pub fn roots(&self) -> &MemoryRoots {
        &self.roots
    }

    /// The opened store handle for one scope.
    ///
    /// A handle is *paths plus scope*, not a cached memory record: every read
    /// still loads the current Markdown file and checks its trust receipt, so
    /// an external or cross-process write is visible at the next read. What is
    /// cached is the expensive, idempotent setup work — directory creation,
    /// canonicalization, the Git boundary check, journal recovery and orphan
    /// cleanup — which a long-lived runtime must not repeat per recall.
    fn markdown_service(
        &self,
        scope: ServiceScope,
    ) -> Result<Arc<v2::MemoryService>, ServiceError> {
        {
            let stores = self
                .stores
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(service) = stores.get(&scope) {
                return Ok(service.clone());
            }
        }
        let roots = self.roots.scope_roots(scope).cloned().ok_or_else(|| {
            ServiceError::Unavailable(format!(
                "the {} memory scope is not configured for this session",
                scope.as_str()
            ))
        })?;
        let service = Arc::new(v2::MemoryService::open(roots).map_err(ServiceError::from)?);
        let mut stores = self
            .stores
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Two callers may race the first open; both did the same recovery and
        // either handle is interchangeable, so keep the first one.
        Ok(stores.entry(scope).or_insert(service).clone())
    }

    /// The current root has never been created. A missing parent is the same
    /// fact: nothing has been written into this scope.
    fn current_root_absent(&self, scope: ServiceScope) -> bool {
        let Some(roots) = self.roots.scope_roots(scope) else {
            return false;
        };
        matches!(
            std::fs::symlink_metadata(&roots.current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    }

    /// Nothing has ever been written into `scope`.
    ///
    /// The layout is still validated on this path: a root pair that can never
    /// be opened is a configuration error, and an empty read would hide it
    /// behind a store that is not there.
    fn never_written(&self, scope: ServiceScope) -> Result<bool, ServiceError> {
        if !self.current_root_absent(scope) {
            return Ok(false);
        }
        if let Some(roots) = self.roots.scope_roots(scope) {
            v2::StorePaths::validate(roots).map_err(store_error)?;
        }
        Ok(true)
    }

    /// Open a scope whose current root already exists.
    ///
    /// `Ok(None)` is an empty store that has never been written. Creating it
    /// on a read would put `<repo>/.leveler` into `git status` for a session
    /// that never stored a record. Writes still go through
    /// [`Self::markdown_service`], which prepares the roots.
    fn existing_markdown(
        &self,
        scope: ServiceScope,
    ) -> Result<Option<Arc<v2::MemoryService>>, ServiceError> {
        if self.never_written(scope)? {
            return Ok(None);
        }
        self.markdown_service(scope).map(Some)
    }

    // ---------------------------------------------------------------- reads

    /// Current records in one scope. `lanes` filters experience entries for a
    /// caller whose lane is closed.
    pub fn list(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        lanes: MemoryLanes,
        limit: Option<usize>,
    ) -> Result<Vec<MemoryView>, ServiceError> {
        let _ = actor;
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(Vec::new());
        };
        let mut records = service.effective_active()?;
        records.retain(|entry| match entry.record.frontmatter.kind {
            v2::MemoryKind::Experience => lanes.experience,
            v2::MemoryKind::UserFact => lanes.user,
        });
        records.sort_by(|a, b| {
            b.record
                .frontmatter
                .updated_at
                .cmp(&a.record.frontmatter.updated_at)
        });
        if let Some(limit) = limit {
            records.truncate(limit);
        }
        Ok(records
            .iter()
            .map(|entry| markdown_view(&entry.record, entry.effective_authority))
            .collect())
    }

    /// Current records for a person's own listing.
    ///
    /// Unlike [`Self::list`], a credential-shaped record keeps its row and its
    /// `sensitive` flag: the user must be able to find it and archive it, while
    /// the caller withholds the text. Model-facing paths never call this.
    pub fn listing_for_user(
        &self,
        scope: ServiceScope,
        lanes: MemoryLanes,
    ) -> Result<Vec<MemoryView>, ServiceError> {
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(Vec::new());
        };
        let summaries = service.list(
            &actor_user(),
            v2::ListQuery {
                scope: Some(scope.to_v2()),
                kind: None,
                status: Some(v2::MemoryStatus::Active),
                limit: None,
                include_external: false,
            },
        )?;
        let mut views = Vec::new();
        for summary in summaries {
            let allowed = match summary.kind {
                v2::MemoryKind::Experience => lanes.experience,
                v2::MemoryKind::UserFact => lanes.user,
            };
            if !allowed {
                continue;
            }
            let record = service.read(&actor_user(), &summary.id)?;
            views.push(markdown_view(&record.record, record.effective_authority));
        }
        Ok(views)
    }

    /// One record by id. Only a current record is returned as current truth.
    pub fn read(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
    ) -> Result<MemoryView, ServiceError> {
        let _ = actor;
        let id =
            v2::MemoryId::parse(id).map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let Some(service) = self.existing_markdown(scope)? else {
            return Err(ServiceError::NotFound(id.to_string()));
        };
        let record = service.read(actor, &id)?;
        if record.record.frontmatter.sensitive {
            return Err(ServiceError::NotFound(id.to_string()));
        }
        let mut view = markdown_view(&record.record, record.effective_authority);
        view.current = true;
        Ok(view)
    }

    /// One record that an automatic injection path may show the model.
    ///
    /// Unlike [`Self::read`], this refuses a Markdown record whose receipt is
    /// not trusted (for example after an external edit). The caller is the
    /// memory context builder; the user's own listing is a different path.
    pub fn read_injectable(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
    ) -> Result<MemoryView, ServiceError> {
        let id =
            v2::MemoryId::parse(id).map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let Some(service) = self.existing_markdown(scope)? else {
            return Err(ServiceError::NotFound(id.to_string()));
        };
        let record = match service.read(actor, &id) {
            Ok(record) => record,
            Err(error) => return Err(error.into()),
        };
        injectable_view(scope, record)
    }

    /// Several records that an automatic injection path may show the model.
    ///
    /// The same trust rules as [`Self::read_injectable`], applied to a whole
    /// candidate set under one store lock and one journal check. Results are
    /// positional; a slot that is not current says so and does not hide its
    /// neighbours.
    pub fn read_many_injectable(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        ids: &[String],
    ) -> Result<Vec<Result<MemoryView, ServiceError>>, ServiceError> {
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(ids
                .iter()
                .map(|id| match v2::MemoryId::parse(id) {
                    Ok(parsed) => Err(ServiceError::NotFound(parsed.to_string())),
                    Err(_) => Err(ServiceError::Invalid(format!("not a memory id: {id}"))),
                })
                .collect());
        };
        let parsed: Vec<Option<v2::MemoryId>> =
            ids.iter().map(|id| v2::MemoryId::parse(id).ok()).collect();
        let known: Vec<v2::MemoryId> = parsed.iter().flatten().cloned().collect();
        let mut outcomes = service.read_many(actor, &known)?.into_iter();
        let mut results = Vec::with_capacity(ids.len());
        for (id, parsed) in ids.iter().zip(parsed) {
            let Some(parsed) = parsed else {
                results.push(Err(ServiceError::Invalid(format!("not a memory id: {id}"))));
                continue;
            };
            let outcome = outcomes
                .next()
                .unwrap_or_else(|| Err(v2::StoreError::NotFound(parsed.clone())));
            results.push(match outcome {
                Ok(record) => injectable_view(scope, record),
                Err(error) => Err(error.into()),
            });
        }
        Ok(results)
    }

    /// Lexical search over the derived index.
    ///
    /// Each hit is revalidated against current Markdown before it is returned,
    /// so a stale row can never surface an old body. A genuine miss is an empty
    /// result; an index or store failure is an explicit error.
    pub fn search(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        lanes: MemoryLanes,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryView, f64)>, ServiceError> {
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(Vec::new());
        };
        let Some(index) = self.fresh_index(scope)? else {
            return Ok(Vec::new());
        };
        let hits = index
            .search(query, limit.max(1).saturating_mul(4), lanes)
            .map_err(index_error)?;
        self.validated_hits(actor, scope, &service, &index, &hits, lanes, limit.max(1))
    }

    /// Revalidate ranked index rows against current Markdown.
    ///
    /// Rows are validated in chunks and validation stops as soon as `wanted`
    /// survive, so a healthy candidate set costs one read per candidate instead
    /// of four. A row that is no longer trusted, current, in scope, unexpired or
    /// digest-matching is dropped from the derived index and never returned: the
    /// index can suggest a candidate, only Markdown can confirm one.
    #[allow(clippy::too_many_arguments)]
    fn validated_hits(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        service: &v2::MemoryService,
        index: &v2::MemoryIndex,
        ranked: &[v2::IndexHit],
        lanes: MemoryLanes,
        wanted: usize,
    ) -> Result<Vec<(MemoryView, f64)>, ServiceError> {
        if wanted == 0 || ranked.is_empty() {
            return Ok(Vec::new());
        }
        let now = chrono::Utc::now();
        let mut views = Vec::new();
        let mut stale = Vec::new();
        let mut cursor = 0;
        while cursor < ranked.len() && views.len() < wanted {
            let end = (cursor + wanted).min(ranked.len());
            let slice = &ranked[cursor..end];
            cursor = end;
            // One store lock and one journal check per chunk.
            let parsed: Vec<Option<v2::MemoryId>> = slice
                .iter()
                .map(|hit| v2::MemoryId::parse(&hit.id).ok())
                .collect();
            let known: Vec<v2::MemoryId> = parsed.iter().flatten().cloned().collect();
            let hit_ids: Vec<&str> = slice.iter().map(|hit| hit.id.as_str()).collect();
            let stored = index.stored_many(&hit_ids).map_err(index_error)?;
            let mut records = service.read_many(actor, &known)?.into_iter();
            for (position, hit) in slice.iter().enumerate() {
                if views.len() >= wanted {
                    break;
                }
                if parsed[position].is_none() {
                    stale.push(hit.id.clone());
                    continue;
                }
                let record = match records.next() {
                    Some(Ok(record)) => record,
                    // The index row points at a file that is no longer current:
                    // drop the derived row, keep Markdown.
                    Some(Err(_)) | None => {
                        stale.push(hit.id.clone());
                        continue;
                    }
                };
                let valid = record.trust == v2::TrustSummary::Trusted
                    && record.record.frontmatter.status.is_active()
                    && !v2::index::is_sensitive_record(&record.record)
                    && record.record.frontmatter.scope == scope.to_v2()
                    && record
                        .record
                        .frontmatter
                        .expires_at
                        .map(|expires| expires > now)
                        .unwrap_or(true)
                    && stored[position].as_ref().is_some_and(|(revision, digest)| {
                        *revision == record.cas.revision && digest == &record.digest
                    });
                if !valid {
                    stale.push(hit.id.clone());
                    continue;
                }
                let view = markdown_view(&record.record, record.effective_authority);
                if !lanes.allows_experience(view.is_experience()) {
                    continue;
                }
                views.push((view, hit.score));
            }
        }
        for id in stale {
            let _ = index.remove(&id);
        }
        Ok(views)
    }

    /// Rebuild the derived index for one scope from Markdown current.
    ///
    /// This is the controlled recovery path after index damage and the cold
    /// start path for a new process. It never writes Markdown.
    pub fn refresh_index(&self, scope: ServiceScope) -> Result<v2::ReconcileReport, ServiceError> {
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(v2::ReconcileReport::default());
        };
        let records = service.effective_active()?;
        self.index_for(scope)?
            .reconcile(&records)
            .map_err(index_error)
    }

    /// Derived-index health, for diagnostics and degradation reporting.
    pub fn index_diagnostics(
        &self,
        scope: ServiceScope,
    ) -> Result<Option<v2::IndexDiagnostics>, ServiceError> {
        if self.never_written(scope)? {
            return Ok(None);
        }
        self.index_for(scope)?
            .diagnostics()
            .map(Some)
            .map_err(index_error)
    }

    /// Catalog candidates for one scope, from the derived index.
    ///
    /// The catalog is a cache-stable list of titles, never content authority:
    /// every entry is active, non-sensitive and user-stated. The index is
    /// reconciled first when it is dirty, so a cold start is a controlled
    /// rebuild rather than an empty prefix.
    pub fn catalog_candidates(
        &self,
        scope: ServiceScope,
        limit: usize,
    ) -> Result<Vec<v2::DerivedRow>, ServiceError> {
        let Some(index) = self.fresh_index(scope)? else {
            return Ok(Vec::new());
        };
        index
            .catalog_rows(scope.to_v2(), limit)
            .map_err(index_error)
    }

    /// Standing candidates for one scope, from the derived index.
    pub fn standing_candidates(
        &self,
        scope: ServiceScope,
        limit: usize,
    ) -> Result<Vec<v2::DerivedRow>, ServiceError> {
        let Some(index) = self.fresh_index(scope)? else {
            return Ok(Vec::new());
        };
        index
            .standing_rows(scope.to_v2(), limit)
            .map_err(index_error)
    }

    fn fresh_index(
        &self,
        scope: ServiceScope,
    ) -> Result<Option<Arc<v2::MemoryIndex>>, ServiceError> {
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(None);
        };
        let index = self.index_for(scope)?;
        if index.is_dirty().map_err(index_error)? {
            let records = service.effective_active()?;
            index.reconcile(&records).map_err(index_error)?;
        }
        Ok(Some(index))
    }

    /// Private state root for one scope, when it is configured. Used by the
    /// catalog owner; it is derived state, never content authority.
    pub fn scope_state_root(&self, scope: ServiceScope) -> Option<&std::path::Path> {
        self.roots
            .scope_roots(scope)
            .map(|roots| roots.state.as_path())
    }

    fn index_for(&self, scope: ServiceScope) -> Result<Arc<v2::MemoryIndex>, ServiceError> {
        let roots = self
            .roots
            .scope_roots(scope)
            .ok_or(ServiceError::NotConfigured)?;
        let mut indexes = self
            .indexes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(index) = indexes.get(&scope) {
            return Ok(index.clone());
        }
        let index =
            Arc::new(v2::MemoryIndex::open(&roots.state.join("index")).map_err(index_error)?);
        indexes.insert(scope, index.clone());
        Ok(index)
    }

    /// Markdown has already committed. Derived-index maintenance must never
    /// turn that success into a failure or roll it back; an index failure is
    /// recorded as dirty so the next retrieval rebuilds before trusting it.
    fn after_markdown_write(
        &self,
        scope: ServiceScope,
        service: &v2::MemoryService,
        outcome: &WriteOutcome,
    ) {
        let Ok(index) = self.index_for(scope) else {
            return;
        };
        match outcome {
            WriteOutcome::Applied { id, superseded, .. } => {
                if let Ok(id) = v2::MemoryId::parse(id) {
                    match service.read(&Actor::External, &id) {
                        Ok(record)
                            if record.trust == v2::TrustSummary::Trusted
                                && record.record.frontmatter.status.is_active()
                                && !v2::index::is_sensitive_record(&record.record) =>
                        {
                            let effective = v2::EffectiveRecord {
                                record: record.record,
                                digest: record.digest,
                                cas: record.cas,
                                effective_authority: record.effective_authority,
                            };
                            let _ = index.sync_record(&effective);
                        }
                        _ => {
                            let _ = index.remove(id.as_str());
                        }
                    }
                }
                for old in superseded {
                    let _ = index.remove(old);
                }
            }
            WriteOutcome::Forgotten { id, .. } => {
                let _ = index.remove(id);
            }
            _ => {}
        }
    }

    /// `(current, archived)` counts.
    pub fn counts(&self, scope: ServiceScope) -> Result<(usize, usize), ServiceError> {
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok((0, 0));
        };
        let active = service.effective_active()?.len();
        let archived = service
            .list(
                &actor_user(),
                v2::ListQuery {
                    scope: Some(scope.to_v2()),
                    kind: None,
                    status: None,
                    limit: None,
                    include_external: false,
                },
            )?
            .into_iter()
            .filter(|summary| !summary.status.is_active())
            .count();
        Ok((active, archived))
    }

    /// Records that are no longer current, for the user's own listing.
    ///
    /// Markdown retires a replaced record out of the current root; the attic
    /// names it and history holds the document. A forgotten record has no
    /// body left, so it does not appear.
    pub fn archived(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        lanes: MemoryLanes,
        limit: Option<usize>,
    ) -> Result<Vec<MemoryView>, ServiceError> {
        let _ = actor;
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(Vec::new());
        };
        let mut records = service.archived()?;
        records.retain(|record| match record.frontmatter.kind {
            v2::MemoryKind::Experience => lanes.experience,
            v2::MemoryKind::UserFact => lanes.user,
        });
        if let Some(limit) = limit {
            records.truncate(limit);
        }
        Ok(records
            .iter()
            .map(|record| {
                // An attic copy is a snapshot of what the file said when it was
                // current, so its own frontmatter can still read `active`. A row in
                // this listing is by construction history: it must never present
                // itself as current truth.
                let mut view = markdown_view(record, record.frontmatter.authority);
                view.current = false;
                if view.status == "active" {
                    view.status = "archived".into();
                }
                view
            })
            .collect())
    }

    // --------------------------------------------------------------- writes

    /// Create or update a memory from a model tool or a trusted user command.
    pub fn remember(
        &self,
        actor: &Actor,
        request: RememberRequest,
    ) -> Result<WriteOutcome, ServiceError> {
        self.remember_markdown(actor, request)
    }

    fn remember_markdown(
        &self,
        actor: &Actor,
        request: RememberRequest,
    ) -> Result<WriteOutcome, ServiceError> {
        let scope = request.scope;
        let service = self.markdown_service(scope)?;
        let intent = request.into_intent()?;
        let admission = service.admit(actor, intent);
        let outcome = markdown_outcome(&service, admission?);
        self.after_markdown_write(scope, &service, &outcome);
        Ok(outcome)
    }

    /// Admit an extracted candidate (user fact or experience) through the
    /// same lifecycle as every other write. `op_seed` must be deterministic for
    /// the durable inbox row so a retry replays instead of duplicating.
    pub fn admit_candidate(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        candidate: &MemoryCandidate,
        op_seed: &str,
    ) -> Result<WriteOutcome, ServiceError> {
        let service = self.markdown_service(scope)?;
        let intent = candidate_intent(candidate, scope, op_seed)?;
        let admission = service.admit(actor, intent)?;
        let outcome = markdown_outcome(&service, admission);
        self.after_markdown_write(scope, &service, &outcome);
        Ok(outcome)
    }

    /// Park a proposal for a person. Used when no approver exists.
    pub fn propose(
        &self,
        actor: &Actor,
        request: RememberRequest,
    ) -> Result<WriteOutcome, ServiceError> {
        let scope = request.scope;
        let service = self.markdown_service(scope)?;
        let intent = request.into_intent()?;
        let reason = match actor {
            Actor::Model { .. } => v2::PendingReason::ScopeRequiresUserCommand,
            Actor::Runtime { .. } => v2::PendingReason::ScopeRequiresUserCommand,
            _ => v2::PendingReason::Other("parked_without_approver".into()),
        };
        // A replacement names a real target; it must run through the
        // lifecycle so the target's authority and CAS are checked. Only
        // a bare create can be parked without a decision.
        let admission = if matches!(intent.relation, v2::Relation::None) {
            service.propose(actor, intent, reason)?
        } else {
            service.admit(actor, intent)?
        };
        let outcome = markdown_outcome(&service, admission);
        self.after_markdown_write(scope, &service, &outcome);
        Ok(outcome)
    }

    /// Forget (archive or tombstone) one record.
    pub fn forget(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
    ) -> Result<WriteOutcome, ServiceError> {
        let service = self.markdown_service(scope)?;
        let memory_id =
            v2::MemoryId::parse(id).map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let record = service.read(actor, &memory_id)?;
        let op = v2::OpId::parse(format!("forget:{id}:{}", record.cas.revision))
            .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let outcome = service.forget(actor, op, &memory_id, record.cas)?;
        let outcome = match outcome {
            v2::ForgetOutcome::Forgotten(tombstone) => WriteOutcome::Forgotten {
                id: tombstone.id.to_string(),
                title: record.record.frontmatter.title,
            },
            v2::ForgetOutcome::Pending(admission) => markdown_outcome(&service, admission),
            v2::ForgetOutcome::NotApplied(admission) => markdown_outcome(&service, admission),
        };
        self.after_markdown_write(scope, &service, &outcome);
        Ok(outcome)
    }

    /// Restore a forgotten legacy entry. Markdown forget is a tombstone and has
    /// no restore path in Stage 2.
    /// Accept or reject one pending record. Only a trusted user invocation may
    /// review; the model never gets a review action.
    pub fn review(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        pending_id: &str,
        decision: ReviewRequest,
    ) -> Result<WriteOutcome, ServiceError> {
        let service = self.markdown_service(scope)?;
        let id = v2::MemoryId::parse(pending_id)
            .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let decision = match decision {
            ReviewRequest::Accept => v2::ReviewDecision::Accept,
            ReviewRequest::Reject { reason } => v2::ReviewDecision::Reject {
                reason: v2::Reason::new(reason),
            },
        };
        let admission = service.review(actor, &id, decision)?;
        let outcome = markdown_outcome(&service, admission);
        self.after_markdown_write(scope, &service, &outcome);
        Ok(outcome)
    }

    /// Update a record the caller may edit. Legacy records have no revision
    /// CAS and no patch path; markdown requires the caller's expected revision.
    pub fn update(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
        expected_revision: u64,
        patch: UpdateRequest,
    ) -> Result<WriteOutcome, ServiceError> {
        // Refuse the two payloads that cannot become a usable record, at the
        // one business entrance every writer shares: a blank title makes the
        // row unfindable for the person who wrote it, and a credential-shaped
        // value must never reach Markdown (the store would reject it later as
        // an opaque backend error; here it is the caller's mistake).
        if let Some(title) = patch.title.as_deref() {
            if title.trim().is_empty() {
                return Err(ServiceError::Invalid("a memory needs a title".to_string()));
            }
            if crate::looks_like_secret(title) {
                return Err(ServiceError::Invalid(
                    "credential-like value in field: title".to_string(),
                ));
            }
        }
        if let Some(body) = patch.body.as_deref()
            && crate::looks_like_secret(body)
        {
            return Err(ServiceError::Invalid(
                "credential-like value in field: body".to_string(),
            ));
        }
        let service = self.markdown_service(scope)?;
        let id =
            v2::MemoryId::parse(id).map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let record = service.read(actor, &id)?;
        if record.cas.revision != expected_revision {
            return Err(ServiceError::Conflict(format!(
                "{id}: current revision {}, expected {expected_revision}",
                record.cas.revision
            )));
        }
        let op = v2::OpId::parse(format!(
            "update:{id}:{expected_revision}:{:x}",
            content_hash(&patch)
        ))
        .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let mut patch = patch.into_patch();
        // A revision is a new write: keep the sources that still
        // support the record and add the operation that produced this
        // revision. Without it the old revision's source would be the
        // only audit trail for new content.
        let mut evidence = record.record.frontmatter.source_refs.clone();
        evidence.push(actor.provenance());
        patch.evidence = Some(evidence);
        let admission = service.update(actor, op, &id, patch, record.cas)?;
        let outcome = markdown_outcome(&service, admission);
        self.after_markdown_write(scope, &service, &outcome);
        Ok(outcome)
    }

    pub fn deprecate(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
        expected_revision: u64,
        reason: String,
    ) -> Result<WriteOutcome, ServiceError> {
        let service = self.markdown_service(scope)?;
        let id =
            v2::MemoryId::parse(id).map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let record = service.read(actor, &id)?;
        if record.cas.revision != expected_revision {
            return Err(ServiceError::Conflict(format!(
                "{id}: current revision {}, expected {expected_revision}",
                record.cas.revision
            )));
        }
        let op = v2::OpId::parse(format!("deprecate:{id}:{expected_revision}"))
            .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let admission = service.deprecate(actor, op, &id, record.cas, v2::Reason::new(reason))?;
        let outcome = markdown_outcome(&service, admission);
        self.after_markdown_write(scope, &service, &outcome);
        Ok(outcome)
    }

    pub fn pin(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
        pinned: bool,
    ) -> Result<WriteOutcome, ServiceError> {
        let service = self.markdown_service(scope)?;
        let id =
            v2::MemoryId::parse(id).map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let record = service.read(actor, &id)?;
        let op = v2::OpId::parse(format!(
            "{}:{id}:{}",
            if pinned { "pin" } else { "unpin" },
            record.cas.revision
        ))
        .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let admission = if pinned {
            service.pin(actor, op, &id, record.cas)?
        } else {
            service.unpin(actor, op, &id, record.cas)?
        };
        let outcome = markdown_outcome(&service, admission);
        self.after_markdown_write(scope, &service, &outcome);
        Ok(outcome)
    }

    // -------------------------------------------------------------- pending

    /// Pending proposals. Only a user or migration path may read them.
    pub fn pending(
        &self,
        actor: &Actor,
        scope: ServiceScope,
    ) -> Result<Vec<MemoryView>, ServiceError> {
        // Authority, not content: an empty store does not make an external
        // actor a reader of the review queue.
        if !matches!(actor, Actor::User { .. } | Actor::Migration { .. }) {
            return Err(ServiceError::NeedsReview(
                "pending memory is only visible to a user review path".into(),
            ));
        }
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(Vec::new());
        };
        Ok(service
            .pending(actor)?
            .iter()
            .map(|record| markdown_view(record, record.frontmatter.authority))
            .collect())
    }

    /// Structured identities the extractor may reference by id.
    pub fn identity_context(
        &self,
        scope: ServiceScope,
        limit: usize,
    ) -> Result<Vec<IdentityContextEntry>, ServiceError> {
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(Vec::new());
        };
        Ok(service
            .identity_context(limit)?
            .into_iter()
            .map(|identity| IdentityContextEntry {
                id: identity.id,
                title: identity.title,
                subject_key: identity.subject_key,
                subject: identity.subject,
                value: identity.value,
                revision: identity.revision,
                kind: match identity.kind {
                    v2::MemoryKind::Experience => "experience".into(),
                    v2::MemoryKind::UserFact => "user_fact".into(),
                },
                authority: identity.authority.as_str().to_string(),
            })
            .collect())
    }

    /// Expire due records and return the records this call actually retired.
    /// The views are captured from the pre-sweep state, so a caller can report
    /// the title of a record that is no longer current.
    pub fn expire_due(
        &self,
        scope: ServiceScope,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<MemoryView>, ServiceError> {
        let Some(service) = self.existing_markdown(scope)? else {
            return Ok(Vec::new());
        };
        let due: Vec<MemoryView> = service
            .effective_active()?
            .into_iter()
            .filter(|entry| {
                entry
                    .record
                    .frontmatter
                    .expires_at
                    .is_some_and(|expires| expires <= now)
            })
            .map(|entry| markdown_view(&entry.record, entry.effective_authority))
            .collect();
        let retired: std::collections::BTreeSet<String> = service
            .expire_due(now)?
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        if let Ok(index) = self.index_for(scope) {
            for id in &retired {
                let _ = index.remove(id);
            }
        }
        Ok(due
            .into_iter()
            .filter(|view| retired.contains(&view.id))
            .collect())
    }

    /// Read-only diagnostics for a scope that may not be openable.
    pub fn diagnose(&self, scope: ServiceScope) -> Result<v2::DiagnosticReport, ServiceError> {
        let roots = self.roots.scope_roots(scope).cloned().ok_or_else(|| {
            ServiceError::Unavailable(format!(
                "the {} memory scope is not configured for this session",
                scope.as_str()
            ))
        })?;
        v2::MemoryService::diagnose(roots).map_err(Into::into)
    }
}

fn index_error(error: v2::IndexError) -> ServiceError {
    ServiceError::Unavailable(error.to_string())
}

fn store_error(error: v2::PathError) -> ServiceError {
    v2::StoreError::Path(error).into()
}

#[derive(Debug, Clone)]
pub enum ReviewRequest {
    Accept,
    Reject { reason: String },
}

#[derive(Debug, Clone, Default)]
pub struct UpdateRequest {
    pub title: Option<String>,
    pub body: Option<String>,
    pub value: Option<String>,
    pub tags: Option<Vec<String>>,
    pub expires_at: Option<Option<String>>,
    /// Give an explicit (keyless) record a structured identity. The string is
    /// the display subject; its canonical form is the subject key. `None`
    /// means the existing identity is unchanged.
    pub subject: Option<String>,
}

/// Stored identity for a caller-supplied subject label. The label is kept for
/// display; identity folds spelling exactly like the automatic extractor, so
/// every writer agrees on which facts are the same.
fn canonical_subject_key(subject: &str) -> String {
    crate::semantic_key_of(&crate::normalize_subject(subject))
}

impl UpdateRequest {
    fn into_patch(self) -> v2::RecordPatch {
        let subject = self.subject.filter(|subject| !subject.trim().is_empty());
        v2::RecordPatch {
            title: self.title,
            body: self.body,
            value: self.value,
            tags: self.tags,
            subject_key: subject.as_deref().map(canonical_subject_key),
            subject,
            expires_at: self.expires_at.map(|value| {
                value
                    .as_deref()
                    .map(|text| {
                        chrono::DateTime::parse_from_rfc3339(text)
                            .ok()
                            .map(|value| value.with_timezone(&chrono::Utc))
                    })
                    .unwrap_or_default()
            }),
            evidence: None,
        }
    }
}

fn content_hash(value: &UpdateRequest) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    value.title.hash(&mut hasher);
    value.body.hash(&mut hasher);
    value.value.hash(&mut hasher);
    value.tags.hash(&mut hasher);
    value.expires_at.hash(&mut hasher);
    // Only mix in a subject when one is requested, so an update without an
    // identity change keeps the operation id it had before subjects existed.
    if let Some(subject) = &value.subject {
        subject.hash(&mut hasher);
    }
    hasher.finish()
}

fn actor_user() -> Actor {
    Actor::External
}

fn markdown_view(record: &v2::MemoryRecord, authority: v2::MemoryAuthority) -> MemoryView {
    MemoryView {
        id: record.frontmatter.id.to_string(),
        title: record.frontmatter.title.clone(),
        body: record.body.clone(),
        tags: record.frontmatter.tags.clone(),
        kind: match record.frontmatter.kind {
            v2::MemoryKind::Experience => "experience".into(),
            v2::MemoryKind::UserFact => "user_fact".into(),
        },
        status: markdown_status(record.frontmatter.status).to_string(),
        value: record.frontmatter.value.clone(),
        subject_key: record.frontmatter.subject_key.clone(),
        authority: authority.as_str().to_string(),
        revision: Some(record.frontmatter.revision),
        current: record.frontmatter.status.is_active(),
        expires_at: record
            .frontmatter
            .expires_at
            .map(|value| value.to_rfc3339()),
        updated_at: Some(record.frontmatter.updated_at.to_rfc3339()),
        pinned_by_user: record.frontmatter.pinned_by == Some(v2::PinOwner::User),
        sensitive: record.frontmatter.sensitive,
        experience: record
            .frontmatter
            .experience
            .as_ref()
            .map(|experience| ExperienceView {
                // Verification is a claim about the text the runtime observed.
                // It survives the two ways that text reaches a person unchanged
                // — the runtime writing it, or a person accepting the runtime's
                // own proposal — and ends the moment someone rewrites it
                // (`user_explicit`), which is a different text with a different
                // author.
                standing: if matches!(
                    authority,
                    v2::MemoryAuthority::RuntimeObserved | v2::MemoryAuthority::UserApproved
                ) && experience.has_resolution()
                {
                    "verified".into()
                } else {
                    "observed".into()
                },
                applies_when: experience.applies_when.clone(),
                invalid_if: experience.invalid_if.clone(),
                evidence: experience
                    .evidence
                    .iter()
                    .map(|evidence| (evidence.kind.clone(), evidence.reference.clone()))
                    .collect(),
            }),
    }
}

/// Whether a read record may reach an automatic injection path, and the view
/// that path sees.
///
/// One rule, one place: standing selection, recall and any future batch reader
/// must agree exactly on what is injectable.
fn injectable_view(
    scope: ServiceScope,
    record: v2::ReadRecord,
) -> Result<MemoryView, ServiceError> {
    if record.trust != v2::TrustSummary::Trusted
        || !record.record.frontmatter.status.is_active()
        || v2::index::is_sensitive_record(&record.record)
        || record.record.frontmatter.scope != scope.to_v2()
    {
        return Err(ServiceError::NotCurrent(
            record.record.frontmatter.id.to_string(),
        ));
    }
    Ok(markdown_view(&record.record, record.effective_authority))
}

fn markdown_status(status: v2::MemoryStatus) -> &'static str {
    match status {
        v2::MemoryStatus::Active => "active",
        v2::MemoryStatus::Pending => "pending",
        v2::MemoryStatus::Superseded => "superseded",
        v2::MemoryStatus::Deprecated => "deprecated",
        v2::MemoryStatus::Expired => "expired",
    }
}

fn markdown_outcome(service: &v2::MemoryService, admission: v2::Admission) -> WriteOutcome {
    match admission {
        v2::Admission::Applied {
            id,
            revision,
            operation,
            possible_duplicate,
        } => {
            let title = service
                .read(&actor_user(), &id)
                .map(|record| record.record.frontmatter.title)
                .unwrap_or_default();
            WriteOutcome::Applied {
                id: id.to_string(),
                title,
                operation: operation.as_str().to_string(),
                revision: Some(revision),
                superseded: Vec::new(),
                replayed: false,
                possible_duplicate: possible_duplicate.iter().map(|id| id.to_string()).collect(),
            }
        }
        v2::Admission::Replayed {
            id,
            revision,
            operation,
        } => {
            let title = service
                .read(&actor_user(), &id)
                .map(|record| record.record.frontmatter.title)
                .unwrap_or_default();
            WriteOutcome::Applied {
                id: id.to_string(),
                title,
                operation: operation.as_str().to_string(),
                revision: Some(revision),
                superseded: Vec::new(),
                replayed: true,
                possible_duplicate: Vec::new(),
            }
        }
        v2::Admission::Pending { id, reason } => WriteOutcome::Pending {
            id: id.to_string(),
            title: service
                .read(&actor_user(), &id)
                .map(|record| record.record.frontmatter.title)
                .unwrap_or_default(),
            reason: reason.as_str().to_string(),
        },
        v2::Admission::Skipped { existing, reason } => WriteOutcome::Skipped {
            existing: existing.to_string(),
            reason: reason.as_str().to_string(),
        },
        v2::Admission::Conflict {
            target,
            current,
            expected,
        } => WriteOutcome::Conflict {
            target: target.to_string(),
            current_revision: current.revision,
            expected_revision: expected.revision,
        },
        v2::Admission::Rejected { reason } => WriteOutcome::Rejected {
            reason: reason.as_str().to_string(),
        },
    }
}

impl RememberRequest {
    fn into_intent(self) -> Result<v2::WriteIntent, ServiceError> {
        let scope = self.scope.to_v2();
        let kind = match self.kind.as_str() {
            "experience" => v2::MemoryKind::Experience,
            _ => v2::MemoryKind::UserFact,
        };
        // A trusted direct command (`记住：…`, `/remember`) has no structured
        // subject: it is stored as a keyless fact, exactly like the legacy
        // direct note. The model tool contract requires `subject` at its own
        // boundary, where the model is the one supplying meaning. The label is
        // folded into the same canonical identity the automatic extractor
        // uses; otherwise two spellings become two active facts.
        let subject_key = self
            .subject
            .as_deref()
            .filter(|subject| !subject.trim().is_empty())
            .map(canonical_subject_key);
        let mut tags = self.tags;
        // The v2 record kind has no preference/decision/note distinction. The
        // declared label stays visible as a tag instead of becoming an
        // undocumented metadata field.
        if !tags.iter().any(|tag| tag == &self.kind)
            && matches!(self.kind.as_str(), "preference" | "decision" | "note")
        {
            tags.push(self.kind.clone());
        }
        let evidence = self
            .evidence
            .filter(|span| !span.trim().is_empty())
            .map(|span| user_turn_source(&span, None, None))
            .into_iter()
            .collect();
        let expires_at = match self.expires_at.as_deref() {
            Some(value) => Some(
                chrono::DateTime::parse_from_rfc3339(value)
                    .map_err(|error| ServiceError::Invalid(format!("invalid expires_at: {error}")))?
                    .with_timezone(&chrono::Utc),
            ),
            None => None,
        };
        let relation = match self.replaces_id.as_deref() {
            Some(id) => v2::Relation::Replaces {
                target: v2::MemoryId::parse(id)
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?,
            },
            None => v2::Relation::None,
        };
        Ok(v2::WriteIntent {
            op_id: v2::OpId::parse(self.op_id)
                .map_err(|error| ServiceError::Invalid(error.to_string()))?,
            kind,
            scope,
            subject: None,
            subject_key,
            value: self.value,
            title: self.title,
            body: self.body,
            tags,
            statement: v2::StatementKind::Assertion,
            relation,
            expires_at,
            evidence,
            experience: None,
            proposed_authority: v2::MemoryAuthority::ModelInferred,
            expected: None,
        })
    }
}

fn candidate_intent(
    candidate: &MemoryCandidate,
    scope: ServiceScope,
    op_seed: &str,
) -> Result<v2::WriteIntent, ServiceError> {
    let kind = if candidate.experience.is_some() {
        v2::MemoryKind::Experience
    } else {
        v2::MemoryKind::UserFact
    };
    let subject_key = candidate
        .semantic_key
        .clone()
        .or_else(|| candidate.key.clone());
    if kind == v2::MemoryKind::UserFact
        && subject_key
            .as_deref()
            .is_none_or(|key| key.trim().is_empty())
    {
        return Err(ServiceError::Invalid(format!(
            "candidate `{}` has no subject key",
            candidate.id
        )));
    }
    let statement = match candidate.operation {
        CandidateOperation::Create => v2::StatementKind::Assertion,
        CandidateOperation::Update => v2::StatementKind::Correction,
        CandidateOperation::Negate => v2::StatementKind::Rejection,
    };
    let relation = match candidate.replaces_id.as_deref() {
        Some(id) => v2::Relation::Replaces {
            target: v2::MemoryId::parse(id)
                .map_err(|error| ServiceError::Invalid(error.to_string()))?,
        },
        None => v2::Relation::None,
    };
    let mut evidence = Vec::new();
    if let Some(span) = candidate.evidence.as_deref()
        && !span.trim().is_empty()
    {
        evidence.push(user_turn_source(
            span,
            candidate.evidence_session.as_deref(),
            candidate.evidence_turn.as_deref(),
        ));
    }
    let expires_at = match candidate.expires_at.as_deref() {
        Some(value) => Some(
            chrono::DateTime::parse_from_rfc3339(value)
                .map_err(|error| ServiceError::Invalid(format!("invalid expires_at: {error}")))?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };
    let op_id = format!(
        "{}:{}",
        op_seed,
        &crate::v2::sha256_hex_text(&candidate.fingerprint)[..16]
    );
    Ok(v2::WriteIntent {
        op_id: v2::OpId::parse(op_id).map_err(|error| ServiceError::Invalid(error.to_string()))?,
        kind,
        scope: scope.to_v2(),
        subject: None,
        subject_key,
        value: candidate.value.clone(),
        title: candidate.title.clone(),
        body: candidate.body.clone(),
        tags: candidate.tags.clone(),
        statement,
        relation,
        expires_at,
        evidence,
        experience: candidate.experience.as_ref().map(v2_experience),
        proposed_authority: v2::MemoryAuthority::ModelInferred,
        expected: None,
    })
}

fn v2_experience(record: &ExperienceRecord) -> v2::Experience {
    v2::Experience {
        category: record.category.as_str().to_string(),
        applies_when: record.applies_when.clone(),
        invalid_if: record.invalid_if.clone(),
        environment: serde_yaml::Mapping::new(),
        evidence: record
            .evidence
            .iter()
            .map(|evidence| v2::ExperienceEvidence {
                kind: evidence.kind.clone(),
                reference: evidence.reference.clone(),
            })
            .collect(),
    }
}

/// The provenance of a fact extracted from a user turn.
///
/// The span is what the user said; the session and turn are which message it
/// was said in. The runtime supplies the identities because it read the turn —
/// an extractor's output never carries them — so a stored `user_stated` fact
/// can always be traced back to a real message instead of only to a string.
fn user_turn_source(span: &str, session: Option<&str>, turn: Option<&str>) -> v2::SourceRef {
    v2::SourceRef {
        kind: v2::SourceRefKind::UserTurn,
        session_id: session.map(str::to_string),
        turn_id: turn.map(str::to_string),
        tool_call_id: None,
        task_id: None,
        pending_id: None,
        evidence_span: Some(span.to_string()),
        message_digest: None,
        span_range: None,
        file_sha256: None,
        legacy_store: None,
        legacy_id: None,
        legacy_sha256: None,
        extra: serde_yaml::Mapping::new(),
    }
}

#[cfg(test)]
mod lazy_open_tests {
    use super::{MemoryRoots, MemoryService, ServiceScope};
    use crate::MemoryLanes;
    use crate::v2::{Actor, MemoryScope, ScopeRoots, UserInvocationId};

    /// A session-open listing reads every configured scope. That read must not
    /// materialize `<repo>/.leveler`, or a repository the user never wrote
    /// memory into shows up dirty in `git status`.
    #[test]
    fn reading_an_uninitialized_scope_does_not_create_it() {
        let repo = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        std::fs::write(repo.path().join("readme.md"), "hi\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=p@e.com",
                "-c",
                "user.name=probe",
                "commit",
                "-qm",
                "fixture",
            ])
            .current_dir(repo.path())
            .status()
            .unwrap();

        let service = MemoryService::new(
            MemoryRoots::new()
                .with_project(ScopeRoots::new(
                    MemoryScope::Project,
                    repo.path().join(".leveler/memory"),
                    state.path().join("project"),
                ))
                .with_local(
                    ScopeRoots::new(
                        MemoryScope::Local,
                        repo.path().join(".leveler/local/memory"),
                        state.path().join("local"),
                    )
                    .with_repo_root(repo.path()),
                ),
        );
        let actor = Actor::User {
            invocation: UserInvocationId::mint("ui:memory-probe"),
        };
        for scope in [ServiceScope::Project, ServiceScope::Local] {
            assert!(
                service
                    .listing_for_user(scope, MemoryLanes::ALL)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                service
                    .archived(&actor, scope, MemoryLanes::ALL, None)
                    .unwrap()
                    .is_empty()
            );
            assert!(service.pending(&actor, scope).unwrap().is_empty());
            assert!(
                service
                    .search(&actor, scope, MemoryLanes::USER_ONLY, "anything", 8)
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            !repo.path().join(".leveler").exists(),
            "a read created {}",
            repo.path().join(".leveler").display()
        );
        let status = std::process::Command::new("git")
            .args(["status", "--short"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git status failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&status.stdout).trim(),
            "",
            "git status is dirty"
        );

        let outcome = service
            .remember(
                &actor,
                super::RememberRequest::new(
                    "op-lazy-1",
                    ServiceScope::Project,
                    "keep",
                    "keep this",
                    "note",
                ),
            )
            .unwrap();
        assert!(outcome.applied_id().is_some(), "{outcome:?}");
        assert!(repo.path().join(".leveler/memory").is_dir());
        assert_eq!(
            service
                .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
                .unwrap()
                .len(),
            1
        );
        assert!(!repo.path().join(".leveler/local").exists());
    }

    /// A read of a scope that was never written answers "empty" only when a
    /// store could exist at those roots at all. A layout that can never be
    /// opened is a configuration error and must not be hidden behind an
    /// empty result — the caller would act on a store that is not there.
    #[test]
    fn reading_an_unopenable_layout_reports_it_instead_of_empty() {
        let repo = tempfile::tempdir().unwrap();
        let service = MemoryService::new(
            MemoryRoots::new().with_project(
                ScopeRoots::new(
                    MemoryScope::Project,
                    repo.path().join(".leveler/memory"),
                    // State inside the repository can never be a store.
                    repo.path().join("state"),
                )
                .with_repo_root(repo.path()),
            ),
        );

        let error = service
            .listing_for_user(ServiceScope::Project, MemoryLanes::ALL)
            .expect_err("an unopenable layout must not read as an empty store");
        assert!(
            error.to_string().contains("inside the repository"),
            "{error}"
        );
        assert!(
            !repo.path().join(".leveler").exists(),
            "reporting the layout must not create it"
        );
    }

    /// The review queue is user authority, not store content. An external
    /// actor asking about a store that was never written must still be
    /// refused, not told there is nothing to review.
    #[test]
    fn pending_review_keeps_its_authority_on_an_empty_store() {
        let repo = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let service = MemoryService::new(MemoryRoots::new().with_project(ScopeRoots::new(
            MemoryScope::Project,
            repo.path().join(".leveler/memory"),
            state.path().join("project"),
        )));

        let error = service
            .pending(&Actor::External, ServiceScope::Project)
            .expect_err("an external actor must not read the review queue");
        assert!(
            matches!(error, super::ServiceError::NeedsReview(_)),
            "{error}"
        );
    }
}
