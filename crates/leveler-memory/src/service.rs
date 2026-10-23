//! Unified memory service.
//!
//! Every production write goes through this facade, whichever storage backend
//! the project selected. The backend distinction is about storage and
//! lifecycle mechanics, not authority: authority is derived from the [`Actor`]
//! path, and no model payload, CLI argument, or Markdown field can construct
//! an [`Actor::User`].
//!
//! `Legacy` delegates to the shipped JSON store so the default behavior does
//! not change. `Markdown` uses the v2 store and its cross-file transaction.
//! Both are reached through the same methods, so the tools, the consolidator,
//! the interactive command handlers and the CLI cannot drift into separate
//! write paths.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use leveler_core::MemoryBackend;

use crate::v2;
use crate::v2::Actor;
use crate::{
    AgentAdmitOutcome, AppliedOperation, CandidateOperation, ExperienceRecord, MemoryCandidate,
    MemoryEntry, MemoryError, MemoryKind as LegacyKind, MemoryLanes, MemoryStore, ProposeOutcome,
};

/// The storage location the service was constructed with.
#[derive(Debug, Clone)]
pub struct MemoryRoots {
    pub backend: MemoryBackend,
    /// Legacy JSON store root.
    pub legacy_root: Option<PathBuf>,
    pub project: Option<v2::ScopeRoots>,
    pub local: Option<v2::ScopeRoots>,
    pub global: Option<v2::ScopeRoots>,
}

impl MemoryRoots {
    /// Legacy JSON store at `root`.
    pub fn legacy(root: impl Into<PathBuf>) -> Self {
        Self {
            backend: MemoryBackend::Legacy,
            legacy_root: Some(root.into()),
            project: None,
            local: None,
            global: None,
        }
    }

    /// Markdown store with no scope configured yet.
    pub fn markdown() -> Self {
        Self {
            backend: MemoryBackend::Markdown,
            legacy_root: None,
            project: None,
            local: None,
            global: None,
        }
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

/// The scope a caller addresses. Legacy storage only has one project-shaped
/// store; the other scopes are unavailable there instead of being silently
/// mapped onto it.
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

impl From<MemoryError> for ServiceError {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::NotFound(id) => Self::NotFound(id),
            MemoryError::Invalid(reason) => Self::Invalid(reason),
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
    /// `user_fact`, `experience`, or a legacy kind label.
    pub kind: String,
    /// `active`, `pending`, `superseded`, `deprecated`, `expired`, `archived`.
    pub status: String,
    pub value: Option<String>,
    pub subject_key: Option<String>,
    pub authority: String,
    pub revision: Option<u64>,
    pub current: bool,
    pub expires_at: Option<String>,
    /// RFC 3339 update time from the content authority; `None` for legacy
    /// candidates that carry no timestamp.
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

    fn legacy_kind(&self) -> Result<LegacyKind, ServiceError> {
        match self.kind.as_str() {
            "preference" => Ok(LegacyKind::Preference),
            "decision" => Ok(LegacyKind::Decision),
            "note" | "user_fact" => Ok(LegacyKind::Note),
            other => Err(ServiceError::Invalid(format!(
                "unsupported memory kind `{other}`"
            ))),
        }
    }
}

/// The unified memory entry point.
pub struct MemoryService {
    roots: MemoryRoots,
    indexes: Mutex<HashMap<ServiceScope, Arc<v2::MemoryIndex>>>,
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
        }
    }

    /// A legacy service rooted at `root`.
    pub fn legacy(root: impl Into<PathBuf>) -> Self {
        Self::new(MemoryRoots::legacy(root))
    }

    pub fn backend(&self) -> MemoryBackend {
        self.roots.backend
    }

    pub fn is_configured(&self) -> bool {
        match self.roots.backend {
            MemoryBackend::Legacy => self.roots.legacy_root.is_some(),
            MemoryBackend::Markdown => {
                self.roots.project.is_some()
                    || self.roots.local.is_some()
                    || self.roots.global.is_some()
            }
        }
    }

    /// The scopes this service can address, in review-surface order.
    ///
    /// A listing covers exactly these stores. Legacy storage has no scope
    /// dimension, so it reports only the project store it actually is.
    pub fn configured_scopes(&self) -> Vec<ServiceScope> {
        match self.roots.backend {
            MemoryBackend::Legacy => {
                if self.roots.legacy_root.is_some() {
                    vec![ServiceScope::Project]
                } else {
                    Vec::new()
                }
            }
            MemoryBackend::Markdown => [
                ServiceScope::Global,
                ServiceScope::Project,
                ServiceScope::Local,
            ]
            .into_iter()
            .filter(|scope| self.roots.scope_roots(*scope).is_some())
            .collect(),
        }
    }

    /// The legacy store root, when this service is on the legacy backend.
    pub fn legacy_root(&self) -> Option<&std::path::Path> {
        self.roots.legacy_root.as_deref()
    }

    pub fn roots(&self) -> &MemoryRoots {
        &self.roots
    }

    fn legacy_store(&self) -> Result<MemoryStore, ServiceError> {
        let root = self
            .roots
            .legacy_root
            .as_ref()
            .ok_or_else(|| ServiceError::NotConfigured)?;
        MemoryStore::open(root).map_err(Into::into)
    }

    fn markdown_service(&self, scope: ServiceScope) -> Result<v2::MemoryService, ServiceError> {
        let roots = self.roots.scope_roots(scope).cloned().ok_or_else(|| {
            ServiceError::Unavailable(format!(
                "the {} memory scope is not configured for this session",
                scope.as_str()
            ))
        })?;
        v2::MemoryService::open(roots).map_err(Into::into)
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
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                let mut entries = store.effective_active()?;
                entries.retain(|entry| !crate::is_sensitive(entry) && lanes.allows_entry(entry));
                entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                if let Some(limit) = limit {
                    entries.truncate(limit);
                }
                Ok(entries.iter().map(legacy_view).collect())
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
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
        }
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
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                let mut entries = store.effective_active()?;
                entries.retain(|entry| lanes.allows_entry(entry));
                entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                Ok(entries.iter().map(legacy_view).collect())
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
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
        }
    }

    /// One record by id. Only a current record is returned as current truth.
    pub fn read(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
    ) -> Result<MemoryView, ServiceError> {
        let _ = actor;
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                let entry = store.read_active(id)?;
                if crate::is_sensitive(&entry) {
                    return Err(ServiceError::NotFound(id.to_string()));
                }
                let current = store
                    .effective_active()?
                    .into_iter()
                    .any(|candidate| candidate.id == entry.id);
                if !current {
                    return Err(ServiceError::NotCurrent(id.to_string()));
                }
                let mut view = legacy_view(&entry);
                view.current = true;
                Ok(view)
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                let id = v2::MemoryId::parse(id)
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                let record = service.read(actor, &id)?;
                if record.record.frontmatter.sensitive {
                    return Err(ServiceError::NotFound(id.to_string()));
                }
                let mut view = markdown_view(&record.record, record.effective_authority);
                view.current = true;
                Ok(view)
            }
        }
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
        match self.roots.backend {
            MemoryBackend::Legacy => self.read(actor, scope, id),
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                let id = v2::MemoryId::parse(id)
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                let record = match service.read(actor, &id) {
                    Ok(record) => record,
                    Err(error) => return Err(error.into()),
                };
                if record.trust != v2::TrustSummary::Trusted
                    || !record.record.frontmatter.status.is_active()
                    || v2::index::is_sensitive_record(&record.record)
                    || record.record.frontmatter.scope != scope.to_v2()
                {
                    return Err(ServiceError::NotCurrent(id.to_string()));
                }
                Ok(markdown_view(&record.record, record.effective_authority))
            }
        }
    }

    /// Lexical or vector search.
    ///
    /// On the Markdown backend both flags run over the derived lexical index;
    /// each hit is revalidated against current Markdown before it is returned,
    /// so a stale row can never surface an old body. A genuine miss is an
    /// empty result; an index or store failure is an explicit error.
    pub fn search(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        lanes: MemoryLanes,
        query: &str,
        limit: usize,
        vector: bool,
    ) -> Result<Vec<(MemoryView, f64)>, ServiceError> {
        let _ = actor;
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                let active_count = store.list_active()?.len();
                let search_limit = active_count.max(limit.max(1));
                let hits = if vector {
                    store.vector_search(query, search_limit)?
                } else {
                    store.search(query, search_limit)?
                };
                let mut views: Vec<(MemoryView, f64)> = hits
                    .into_iter()
                    .filter(|(entry, _)| !crate::is_sensitive(entry) && lanes.allows_entry(entry))
                    .map(|(entry, score)| (legacy_view(&entry), score))
                    .collect();
                views.truncate(limit.max(1));
                Ok(views)
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                let index = self.index_for(scope)?;
                if index.is_dirty().map_err(index_error)? {
                    let records = service.effective_active()?;
                    index.reconcile(&records).map_err(index_error)?;
                }
                let hits = index
                    .search(query, limit.max(1).saturating_mul(4))
                    .map_err(index_error)?;
                let now = chrono::Utc::now();
                let mut views = Vec::new();
                for hit in hits {
                    let Ok(id) = v2::MemoryId::parse(&hit.id) else {
                        continue;
                    };
                    let record = match service.read(actor, &id) {
                        Ok(record) => record,
                        Err(_) => {
                            // The index row points at a file that is no longer
                            // current: drop the derived row, keep Markdown.
                            let _ = index.remove(&hit.id);
                            continue;
                        }
                    };
                    let stored = index.stored(&hit.id).map_err(index_error)?;
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
                        && stored.as_ref().is_some_and(|(revision, digest)| {
                            *revision == record.cas.revision && digest == &record.digest
                        });
                    if !valid {
                        // A stale derived row may never surface an old body.
                        let _ = index.remove(&hit.id);
                        continue;
                    }
                    let view = markdown_view(&record.record, record.effective_authority);
                    if !lanes.allows_experience(view.is_experience()) {
                        continue;
                    }
                    views.push((view, hit.score));
                    if views.len() >= limit.max(1) {
                        break;
                    }
                }
                Ok(views)
            }
        }
    }

    /// Rebuild the derived index for one scope from Markdown current.
    ///
    /// This is the controlled recovery path after index damage and the cold
    /// start path for a new process. It never writes Markdown.
    pub fn refresh_index(&self, scope: ServiceScope) -> Result<v2::ReconcileReport, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => Ok(v2::ReconcileReport::default()),
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                let records = service.effective_active()?;
                self.index_for(scope)?
                    .reconcile(&records)
                    .map_err(index_error)
            }
        }
    }

    /// Derived-index health, for diagnostics and degradation reporting.
    pub fn index_diagnostics(
        &self,
        scope: ServiceScope,
    ) -> Result<Option<v2::IndexDiagnostics>, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => Ok(None),
            MemoryBackend::Markdown => self
                .index_for(scope)?
                .diagnostics()
                .map(Some)
                .map_err(index_error),
        }
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
        match self.roots.backend {
            MemoryBackend::Legacy => Ok(Vec::new()),
            MemoryBackend::Markdown => self
                .fresh_index(scope)?
                .catalog_rows(scope.to_v2(), limit)
                .map_err(index_error),
        }
    }

    /// Standing candidates for one scope, from the derived index.
    pub fn standing_candidates(
        &self,
        scope: ServiceScope,
        limit: usize,
    ) -> Result<Vec<v2::DerivedRow>, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => Ok(Vec::new()),
            MemoryBackend::Markdown => self
                .fresh_index(scope)?
                .standing_rows(scope.to_v2(), limit)
                .map_err(index_error),
        }
    }

    fn fresh_index(&self, scope: ServiceScope) -> Result<Arc<v2::MemoryIndex>, ServiceError> {
        let service = self.markdown_service(scope)?;
        let index = self.index_for(scope)?;
        if index.is_dirty().map_err(index_error)? {
            let records = service.effective_active()?;
            index.reconcile(&records).map_err(index_error)?;
        }
        Ok(index)
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
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                Ok(self.legacy_store()?.counts()?)
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
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
        }
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
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                let mut entries = store.list_archived()?;
                entries.retain(|entry| lanes.allows_entry(entry));
                entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                if let Some(limit) = limit {
                    entries.truncate(limit);
                }
                Ok(entries.iter().map(legacy_view).collect())
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
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
                    .map(|record| markdown_view(record, record.frontmatter.authority))
                    .collect())
            }
        }
    }

    // --------------------------------------------------------------- writes

    /// Create or update a memory from a model tool or a trusted user command.
    pub fn remember(
        &self,
        actor: &Actor,
        request: RememberRequest,
    ) -> Result<WriteOutcome, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => self.remember_legacy(actor, request),
            MemoryBackend::Markdown => self.remember_markdown(actor, request),
        }
    }

    fn remember_legacy(
        &self,
        actor: &Actor,
        request: RememberRequest,
    ) -> Result<WriteOutcome, ServiceError> {
        require_project(request.scope)?;
        let store = self.legacy_store()?;
        let kind = request.legacy_kind()?;
        if request.title.trim().is_empty() || request.body.trim().is_empty() {
            return Err(ServiceError::Invalid("title and body are required".into()));
        }
        match actor {
            Actor::Model { .. } | Actor::External => {
                // A model write never claims the user said it. The CLI has no
                // way to mint a user actor, so it follows the same path here.
                let subject = request.subject.as_deref().ok_or_else(|| {
                    ServiceError::Invalid("a model memory write requires a subject".into())
                })?;
                let candidate = crate::agent_candidate(
                    &request.title,
                    &request.body,
                    kind,
                    request.tags,
                    subject,
                    request.evidence,
                )
                .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                Ok(legacy_agent_outcome(
                    store.admit_agent_candidate(candidate)?,
                ))
            }
            Actor::User { .. } => {
                let entry = store.activate(&request.title, &request.body, kind, request.tags)?;
                Ok(WriteOutcome::Applied {
                    id: entry.id,
                    title: entry.title,
                    operation: "created".into(),
                    revision: None,
                    superseded: Vec::new(),
                    replayed: false,
                    possible_duplicate: Vec::new(),
                })
            }
            Actor::Migration { .. } => Err(ServiceError::Unavailable(
                "migration writes are not supported on the legacy backend".into(),
            )),
            Actor::Runtime { .. } => Err(ServiceError::Invalid(
                "runtime extraction writes go through admit_candidate".into(),
            )),
        }
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
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                if candidate.experience.is_some() {
                    Ok(legacy_agent_outcome(
                        store.admit_experience_candidate(candidate.clone())?,
                    ))
                } else {
                    Ok(legacy_admit_outcome(
                        store.admit_candidate(candidate.clone())?,
                    ))
                }
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                let intent = candidate_intent(candidate, scope, op_seed)?;
                let admission = service.admit(actor, intent)?;
                let outcome = markdown_outcome(&service, admission);
                self.after_markdown_write(scope, &service, &outcome);
                Ok(outcome)
            }
        }
    }

    /// Park a proposal for a person. Used when no approver exists.
    pub fn propose(
        &self,
        actor: &Actor,
        request: RememberRequest,
    ) -> Result<WriteOutcome, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(request.scope)?;
                let store = self.legacy_store()?;
                let kind = request.legacy_kind()?;
                let candidate = match request.subject.as_deref() {
                    Some(subject) if !subject.trim().is_empty() => crate::agent_candidate(
                        &request.title,
                        &request.body,
                        kind,
                        request.tags,
                        subject,
                        request.evidence,
                    )
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?,
                    _ => MemoryCandidate::new(
                        &request.title,
                        &request.body,
                        kind.candidate_kind(),
                        None,
                        crate::CandidateSource::AgentProposed,
                        request.tags,
                    )
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?,
                };
                match store.propose(candidate)? {
                    ProposeOutcome::Pending(candidate) => Ok(WriteOutcome::Pending {
                        id: candidate.id,
                        title: candidate.title,
                        reason: "awaiting_user_review".into(),
                    }),
                    ProposeOutcome::AlreadyPending(candidate) => Ok(WriteOutcome::Pending {
                        id: candidate.id,
                        title: candidate.title,
                        reason: "already_pending".into(),
                    }),
                    ProposeOutcome::AlreadyActive { id } => Ok(WriteOutcome::Skipped {
                        existing: id,
                        reason: "already_active".into(),
                    }),
                    ProposeOutcome::Suppressed { fingerprint } => Ok(WriteOutcome::NotStored {
                        reason: format!("rejected_previously:{fingerprint}"),
                    }),
                }
            }
            MemoryBackend::Markdown => {
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
        }
    }

    /// Forget (archive or tombstone) one record.
    pub fn forget(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
    ) -> Result<WriteOutcome, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                let entry = store.read_active(id)?;
                if crate::is_sensitive(&entry) {
                    return Err(ServiceError::NotFound(id.to_string()));
                }
                let entry = store.forget(id)?;
                Ok(WriteOutcome::Forgotten {
                    id: entry.id,
                    title: entry.title,
                })
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                let memory_id = v2::MemoryId::parse(id)
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
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
                    v2::ForgetOutcome::NotApplied(admission) => {
                        markdown_outcome(&service, admission)
                    }
                };
                self.after_markdown_write(scope, &service, &outcome);
                Ok(outcome)
            }
        }
    }

    /// Restore a forgotten legacy entry. Markdown forget is a tombstone and has
    /// no restore path in Stage 2.
    pub fn restore(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
    ) -> Result<WriteOutcome, ServiceError> {
        let Actor::User { .. } = actor else {
            return Err(ServiceError::NeedsReview(
                "only a trusted user invocation may restore forgotten memory".into(),
            ));
        };
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                let entry = store.restore(id)?;
                Ok(WriteOutcome::Applied {
                    id: entry.id,
                    title: entry.title,
                    operation: "restored".into(),
                    revision: None,
                    superseded: Vec::new(),
                    replayed: false,
                    possible_duplicate: Vec::new(),
                })
            }
            MemoryBackend::Markdown => Err(ServiceError::Unavailable(
                "restore is not available on the markdown backend".into(),
            )),
        }
    }

    /// Accept or reject one pending record. Only a trusted user invocation may
    /// review; the model never gets a review action.
    pub fn review(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        pending_id: &str,
        decision: ReviewRequest,
    ) -> Result<WriteOutcome, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let Actor::User { .. } = actor else {
                    return Err(ServiceError::NeedsReview(
                        "only a trusted user review may accept or reject pending memory".into(),
                    ));
                };
                let store = self.legacy_store()?;
                match decision {
                    ReviewRequest::Accept => {
                        let entry = store.accept(pending_id)?;
                        Ok(WriteOutcome::Applied {
                            id: entry.id,
                            title: entry.title,
                            operation: "accepted".into(),
                            revision: None,
                            superseded: Vec::new(),
                            replayed: false,
                            possible_duplicate: Vec::new(),
                        })
                    }
                    ReviewRequest::Reject { reason: _ } => {
                        // The legacy store records a suppression fingerprint and
                        // has no field for a free-text reason.
                        store.reject(pending_id)?;
                        Ok(WriteOutcome::Applied {
                            id: pending_id.to_string(),
                            title: String::new(),
                            operation: "rejected".into(),
                            revision: None,
                            superseded: Vec::new(),
                            replayed: false,
                            possible_duplicate: Vec::new(),
                        })
                    }
                }
            }
            MemoryBackend::Markdown => {
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
        }
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
        match self.roots.backend {
            MemoryBackend::Legacy => Err(ServiceError::Unavailable(
                "update is not available on the legacy backend".into(),
            )),
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                let id = v2::MemoryId::parse(id)
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
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
        }
    }

    pub fn deprecate(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
        expected_revision: u64,
        reason: String,
    ) -> Result<WriteOutcome, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => Err(ServiceError::Unavailable(
                "deprecate is not available on the legacy backend".into(),
            )),
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                let id = v2::MemoryId::parse(id)
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                let record = service.read(actor, &id)?;
                if record.cas.revision != expected_revision {
                    return Err(ServiceError::Conflict(format!(
                        "{id}: current revision {}, expected {expected_revision}",
                        record.cas.revision
                    )));
                }
                let op = v2::OpId::parse(format!("deprecate:{id}:{expected_revision}"))
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                let admission =
                    service.deprecate(actor, op, &id, record.cas, v2::Reason::new(reason))?;
                let outcome = markdown_outcome(&service, admission);
                self.after_markdown_write(scope, &service, &outcome);
                Ok(outcome)
            }
        }
    }

    pub fn pin(
        &self,
        actor: &Actor,
        scope: ServiceScope,
        id: &str,
        pinned: bool,
    ) -> Result<WriteOutcome, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => Err(ServiceError::Unavailable(
                "pin is not available on the legacy backend".into(),
            )),
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                let id = v2::MemoryId::parse(id)
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
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
        }
    }

    // -------------------------------------------------------------- pending

    /// Pending proposals. Only a user or migration path may read them.
    pub fn pending(
        &self,
        actor: &Actor,
        scope: ServiceScope,
    ) -> Result<Vec<MemoryView>, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                Ok(store
                    .list_pending()?
                    .iter()
                    .map(legacy_candidate_view)
                    .collect())
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
                Ok(service
                    .pending(actor)?
                    .iter()
                    .map(|record| markdown_view(record, record.frontmatter.authority))
                    .collect())
            }
        }
    }

    /// Structured identities the extractor may reference by id.
    pub fn identity_context(
        &self,
        scope: ServiceScope,
        limit: usize,
    ) -> Result<Vec<IdentityContextEntry>, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                let mut entries: Vec<MemoryEntry> = store
                    .effective_active()?
                    .into_iter()
                    .filter(|entry| !crate::is_sensitive(entry))
                    .collect();
                entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                Ok(entries
                    .into_iter()
                    .take(limit.max(1))
                    .map(|entry| {
                        let kind = LegacyKind::of(&entry).as_str().to_string();
                        let authority = entry.authority().as_str().to_string();
                        IdentityContextEntry {
                            id: entry.id,
                            title: entry.title,
                            subject_key: entry.key.clone(),
                            subject: None,
                            value: entry.value,
                            revision: 0,
                            kind,
                            authority,
                        }
                    })
                    .collect())
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
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
        }
    }

    /// Expire due records and return the records this call actually retired.
    /// The views are captured from the pre-sweep state, so a caller can report
    /// the title of a record that is no longer current.
    pub fn expire_due(
        &self,
        scope: ServiceScope,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<MemoryView>, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Legacy => {
                require_project(scope)?;
                let store = self.legacy_store()?;
                Ok(store.expire_due()?.iter().map(legacy_view).collect())
            }
            MemoryBackend::Markdown => {
                let service = self.markdown_service(scope)?;
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
        }
    }

    /// Read-only diagnostics for a scope that may not be openable.
    pub fn diagnose(&self, scope: ServiceScope) -> Result<v2::DiagnosticReport, ServiceError> {
        match self.roots.backend {
            MemoryBackend::Markdown => {
                let roots = self.roots.scope_roots(scope).cloned().ok_or_else(|| {
                    ServiceError::Unavailable(format!(
                        "the {} memory scope is not configured for this session",
                        scope.as_str()
                    ))
                })?;
                v2::MemoryService::diagnose(roots).map_err(Into::into)
            }
            MemoryBackend::Legacy => Err(ServiceError::Unavailable(
                "structured diagnostics are only available on the markdown backend".into(),
            )),
        }
    }
}

/// Marker kept for clients built against Stage 2; Markdown search now works.
pub const MARKDOWN_SEARCH_UNAVAILABLE: &str = "not_available_until_stage_3";

fn index_error(error: v2::IndexError) -> ServiceError {
    ServiceError::Unavailable(error.to_string())
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
    /// Give an explicit (keyless) record a structured identity. The same
    /// string is the display subject and the subject key; `None` means the
    /// existing identity is unchanged.
    pub subject: Option<String>,
}

impl UpdateRequest {
    fn into_patch(self) -> v2::RecordPatch {
        let subject = self.subject.filter(|subject| !subject.trim().is_empty());
        v2::RecordPatch {
            title: self.title,
            body: self.body,
            value: self.value,
            tags: self.tags,
            subject: subject.clone(),
            subject_key: subject,
            expires_at: match self.expires_at {
                Some(value) => Some(
                    value
                        .as_deref()
                        .map(|text| {
                            chrono::DateTime::parse_from_rfc3339(text)
                                .ok()
                                .map(|value| value.with_timezone(&chrono::Utc))
                        })
                        .unwrap_or_default(),
                ),
                None => None,
            },
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

fn require_project(scope: ServiceScope) -> Result<(), ServiceError> {
    match scope {
        ServiceScope::Project => Ok(()),
        other => Err(ServiceError::Unavailable(format!(
            "the legacy backend only stores project memory; {} scope is unavailable",
            other.as_str()
        ))),
    }
}

fn actor_user() -> Actor {
    Actor::External
}

fn legacy_view(entry: &MemoryEntry) -> MemoryView {
    MemoryView {
        id: entry.id.clone(),
        title: entry.title.clone(),
        body: entry.body.clone(),
        tags: entry.tags.clone(),
        kind: LegacyKind::of(entry).as_str().to_string(),
        status: legacy_status(entry.effective_status()).to_string(),
        value: entry.value.clone(),
        subject_key: entry.key.clone(),
        authority: entry.authority().as_str().to_string(),
        revision: None,
        current: entry.effective_status() == crate::MemoryStatus::Active,
        expires_at: entry.expires_at.clone(),
        updated_at: Some(entry.updated_at.clone()),
        pinned_by_user: false,
        sensitive: crate::is_sensitive(entry),
        experience: entry.experience.as_ref().map(legacy_experience_view),
    }
}

fn legacy_candidate_view(candidate: &MemoryCandidate) -> MemoryView {
    MemoryView {
        id: candidate.id.clone(),
        title: candidate.title.clone(),
        body: candidate.body.clone(),
        tags: candidate.tags.clone(),
        kind: if candidate.experience.is_some() {
            "experience".into()
        } else {
            "user_fact".into()
        },
        status: "pending".into(),
        value: candidate.value.clone(),
        subject_key: candidate
            .semantic_key
            .clone()
            .or_else(|| candidate.key.clone()),
        authority: legacy_authority(candidate.authority).to_string(),
        revision: None,
        current: false,
        expires_at: candidate.expires_at.clone(),
        updated_at: None,
        pinned_by_user: false,
        sensitive: crate::is_sensitive_candidate(candidate),
        experience: candidate.experience.as_ref().map(legacy_experience_view),
    }
}

fn legacy_status(status: crate::MemoryStatus) -> &'static str {
    match status {
        crate::MemoryStatus::Active => "active",
        crate::MemoryStatus::Superseded => "superseded",
        crate::MemoryStatus::Expired => "expired",
        crate::MemoryStatus::Archived => "archived",
    }
}

fn legacy_authority(authority: crate::MemoryAuthority) -> &'static str {
    authority.as_str()
}

fn legacy_experience_view(record: &ExperienceRecord) -> ExperienceView {
    ExperienceView {
        standing: record.standing().as_str().to_string(),
        applies_when: record.applies_when.clone(),
        invalid_if: record.invalid_if.clone(),
        evidence: record
            .evidence
            .iter()
            .map(|evidence| (evidence.kind.clone(), evidence.reference.clone()))
            .collect(),
    }
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
                standing: if experience.has_resolution() {
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

fn markdown_status(status: v2::MemoryStatus) -> &'static str {
    match status {
        v2::MemoryStatus::Active => "active",
        v2::MemoryStatus::Pending => "pending",
        v2::MemoryStatus::Superseded => "superseded",
        v2::MemoryStatus::Deprecated => "deprecated",
        v2::MemoryStatus::Expired => "expired",
    }
}

fn legacy_agent_outcome(outcome: AgentAdmitOutcome) -> WriteOutcome {
    match outcome {
        AgentAdmitOutcome::Applied(commit) => commit_outcome(commit),
        AgentAdmitOutcome::Queued { candidate, reason } => WriteOutcome::Pending {
            id: candidate.id,
            title: candidate.title,
            reason,
        },
        AgentAdmitOutcome::NotStored { reason } => WriteOutcome::NotStored { reason },
    }
}

fn legacy_admit_outcome(outcome: crate::AdmitOutcome) -> WriteOutcome {
    match outcome {
        crate::AdmitOutcome::Applied(commit) => commit_outcome(commit),
        crate::AdmitOutcome::Proposed(outcome) => match outcome {
            ProposeOutcome::Pending(candidate) => WriteOutcome::Pending {
                id: candidate.id,
                title: candidate.title,
                reason: "awaiting_user_review".into(),
            },
            ProposeOutcome::AlreadyPending(candidate) => WriteOutcome::Pending {
                id: candidate.id,
                title: candidate.title,
                reason: "already_pending".into(),
            },
            ProposeOutcome::AlreadyActive { id } => WriteOutcome::Skipped {
                existing: id,
                reason: "already_active".into(),
            },
            ProposeOutcome::Suppressed { fingerprint } => WriteOutcome::NotStored {
                reason: format!("rejected_previously:{fingerprint}"),
            },
        },
    }
}

fn commit_outcome(commit: crate::CommitOutcome) -> WriteOutcome {
    let entry = commit.entry.clone();
    match commit.operation {
        AppliedOperation::Created | AppliedOperation::Superseded | AppliedOperation::Merged => {
            let Some(entry) = entry else {
                return WriteOutcome::NotStored {
                    reason: "memory lifecycle applied no entry".into(),
                };
            };
            WriteOutcome::Applied {
                id: entry.id,
                title: entry.title,
                operation: match commit.operation {
                    AppliedOperation::Created => "created",
                    AppliedOperation::Superseded => "superseded",
                    AppliedOperation::Merged => "merged",
                    _ => "applied",
                }
                .into(),
                revision: None,
                superseded: commit.superseded.iter().map(|old| old.id.clone()).collect(),
                replayed: false,
                possible_duplicate: Vec::new(),
            }
        }
        AppliedOperation::Skipped => WriteOutcome::Skipped {
            existing: entry.map(|entry| entry.id).unwrap_or_default(),
            reason: commit.skipped_reason.unwrap_or_else(|| "skipped".into()),
        },
        AppliedOperation::NeedsApproval => WriteOutcome::Pending {
            id: entry.map(|entry| entry.id).unwrap_or_default(),
            title: String::new(),
            reason: "needs_approval".into(),
        },
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
        // boundary, where the model is the one supplying meaning.
        let subject_key = self.subject.filter(|subject| !subject.trim().is_empty());
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
            .map(|span| user_turn_source(&span))
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
    if let Some(span) = candidate.evidence.as_deref() {
        if !span.trim().is_empty() {
            evidence.push(user_turn_source(span));
        }
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
        crate::v2::sha256_hex_text(&candidate.fingerprint)[..16].to_string()
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

fn user_turn_source(span: &str) -> v2::SourceRef {
    v2::SourceRef {
        kind: v2::SourceRefKind::UserTurn,
        session_id: None,
        turn_id: None,
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
