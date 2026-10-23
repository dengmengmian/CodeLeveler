//! Unified write service for Markdown memory.
//!
//! The store speaks in [`Caller`]s; the service speaks in [`Actor`]s that carry
//! the identities a real runtime has. The distinction is the security boundary:
//! an [`Actor::User`] cannot be produced from a model payload, a YAML field, or
//! a CLI argument, because none of those can construct [`UserInvocationId`].
//! Authority is derived here from the actor path, never from the intent's
//! advisory `proposed_authority`.

use std::cmp::Reverse;

use leveler_core::{SessionId, TaskId, ToolCallId, TurnId};

use super::lifecycle::{Admission, PendingReason};
use super::path::ScopeRoots;
use super::record::{
    Caller, Cas, Experience, MemoryAuthority, MemoryId, MemoryKind, MemoryRecord, MemoryScope,
    OpId, PinOwner, Relation, SourceRef, SourceRefKind, StatementKind,
};
use super::store::{
    AdmitRequest, DiagnosticReport, ForgetOutcome, ListQuery, MarkdownStore, ReadRecord, Reason,
    RecordPatch, ReviewDecision, StoreError,
};

/// Minted once per explicit user interaction command.
///
/// Deliberately not `Deserialize` and without `From<String>`: only in-process
/// trusted command handlers call [`UserInvocationId::mint`]. This is a domain
/// boundary, not OS isolation — a model with shell access can call the CLI, but
/// the CLI cannot turn its argv into this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInvocationId(String);

impl UserInvocationId {
    /// Create the identity of one trusted user command.
    ///
    /// Call this only from a command handler that received the command from a
    /// user interaction surface (TUI command, TUI review key, direct message).
    pub fn mint(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identity of one migration run. Not derived from model text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationId(String);

impl MigrationId {
    pub fn mint(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who is asking. Constructing [`Actor::User`] requires a [`UserInvocationId`];
/// there is no way to deserialize any variant from model JSON.
#[derive(Debug, Clone)]
pub enum Actor {
    User {
        invocation: UserInvocationId,
    },
    Model {
        session: SessionId,
        turn: TurnId,
        tool_call: ToolCallId,
    },
    Runtime {
        task: TaskId,
    },
    Migration {
        run: MigrationId,
    },
    External,
}

impl Actor {
    /// The store-level caller this actor maps to for a normal write.
    fn caller(&self) -> Caller {
        match self {
            Self::User { invocation } => Caller::UserCommand {
                invocation: invocation.as_str().to_string(),
            },
            Self::Model { tool_call, .. } => Caller::Model {
                tool_call: tool_call.as_str().to_string(),
            },
            Self::Runtime { task } => Caller::Runtime {
                task: task.as_str().to_string(),
            },
            Self::Migration { run } => Caller::Migration {
                run: run.as_str().to_string(),
            },
            Self::External => Caller::External,
        }
    }

    /// The provenance the service records for this actor, ignoring anything the
    /// caller may have supplied about who it is.
    ///
    /// Marked `channel: true`: it identifies the write channel, not evidence
    /// for the fact, so the lifecycle must not count it as new evidence.
    pub(crate) fn provenance(&self) -> SourceRef {
        let mut source = empty_source(match self {
            Self::User { .. } => SourceRefKind::UserCommand,
            Self::Model { .. } => SourceRefKind::ToolCall,
            Self::Runtime { .. } => SourceRefKind::RuntimeTask,
            Self::Migration { .. } => SourceRefKind::Migration,
            Self::External => SourceRefKind::ExternalEdit,
        });
        source.extra.insert(
            serde_yaml::Value::String("channel".into()),
            serde_yaml::Value::Bool(true),
        );
        match self {
            Self::User { invocation } => {
                source.extra.insert(
                    serde_yaml::Value::String("invocation".into()),
                    serde_yaml::Value::String(invocation.as_str().to_string()),
                );
            }
            Self::Model {
                session,
                turn,
                tool_call,
            } => {
                source.session_id = Some(session.as_str().to_string());
                source.turn_id = Some(turn.as_str().to_string());
                source.tool_call_id = Some(tool_call.as_str().to_string());
            }
            Self::Runtime { task } => {
                source.task_id = Some(task.as_str().to_string());
            }
            Self::Migration { run } => {
                source.extra.insert(
                    serde_yaml::Value::String("migration_run".into()),
                    serde_yaml::Value::String(run.as_str().to_string()),
                );
            }
            Self::External => {}
        }
        source
    }
}

fn empty_source(kind: SourceRefKind) -> SourceRef {
    SourceRef {
        kind,
        session_id: None,
        turn_id: None,
        tool_call_id: None,
        task_id: None,
        pending_id: None,
        evidence_span: None,
        message_digest: None,
        span_range: None,
        file_sha256: None,
        legacy_store: None,
        legacy_id: None,
        legacy_sha256: None,
        extra: serde_yaml::Mapping::new(),
    }
}

/// A service-level write intent: the store's `MemoryIntent` plus the explicit
/// operation identity used for idempotent retries.
#[derive(Debug, Clone)]
pub struct WriteIntent {
    pub op_id: OpId,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub subject: Option<String>,
    pub subject_key: Option<String>,
    pub value: Option<String>,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub statement: StatementKind,
    pub relation: Relation,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub evidence: Vec<SourceRef>,
    pub experience: Option<Experience>,
    /// Advisory only. The service derives authority from the actor path and
    /// never reads this value.
    pub proposed_authority: MemoryAuthority,
    pub expected: Option<Cas>,
}

impl WriteIntent {
    fn into_store_intent(self, evidence: Vec<SourceRef>) -> super::record::MemoryIntent {
        super::record::MemoryIntent {
            kind: self.kind,
            scope: self.scope,
            subject: self.subject,
            subject_key: self.subject_key,
            value: self.value,
            title: self.title,
            body: self.body,
            tags: self.tags,
            statement: self.statement,
            relation: self.relation,
            expires_at: self.expires_at,
            evidence,
            experience: self.experience,
            proposed_authority: self.proposed_authority,
        }
    }
}

/// One active identity exposed to the extractor. Structured fields only; the
/// service never derives an identity from text.
#[derive(Debug, Clone, PartialEq)]
pub struct IdentityRef {
    pub id: String,
    pub title: String,
    pub subject_key: Option<String>,
    pub subject: Option<String>,
    pub value: Option<String>,
    pub revision: u64,
    pub kind: MemoryKind,
    pub authority: MemoryAuthority,
}

/// Service-level cap for an extractor identity list.
pub const IDENTITY_CONTEXT_LIMIT: usize = 24;

/// The Markdown backend. One instance is bound to one scope; every mutation
/// goes through the store's cross-file transaction.
pub struct MemoryService {
    store: MarkdownStore,
}

impl MemoryService {
    pub fn open(roots: ScopeRoots) -> Result<Self, StoreError> {
        MarkdownStore::open(roots).map(|store| Self { store })
    }

    pub fn scope(&self) -> MemoryScope {
        self.store.scope()
    }

    /// Read-only diagnostics for a scope that may not be openable.
    pub fn diagnose(roots: ScopeRoots) -> Result<DiagnosticReport, StoreError> {
        MarkdownStore::diagnose(roots)
    }

    pub fn read(&self, _actor: &Actor, id: &MemoryId) -> Result<ReadRecord, StoreError> {
        self.store.read(id)
    }

    pub fn history(&self, id: &MemoryId, revision: u64) -> Result<MemoryRecord, StoreError> {
        self.store.history(id, revision)
    }

    /// Retired records, newest first. Read-only; the current root is untouched.
    pub fn archived(&self) -> Result<Vec<MemoryRecord>, StoreError> {
        self.store.archived()
    }

    pub fn list(
        &self,
        _actor: &Actor,
        query: ListQuery,
    ) -> Result<Vec<super::store::MemorySummary>, StoreError> {
        self.store.list(query)
    }

    pub fn effective_active(&self) -> Result<Vec<super::store::EffectiveRecord>, StoreError> {
        self.store.effective_active()
    }

    /// Pending records are private review state; a model never sees them in a
    /// tool result, so only user and migration paths may read them.
    pub fn pending(&self, actor: &Actor) -> Result<Vec<MemoryRecord>, StoreError> {
        match actor {
            Actor::User { .. } | Actor::Migration { .. } => self.store.list_pending(),
            _ => Err(StoreError::ForbiddenActor(
                "pending memory is only visible to a user review path".into(),
            )),
        }
    }

    /// Admit one write. Authority comes from `actor`; `intent.proposed_authority`
    /// is advisory and ignored. The store re-checks identity, protected targets
    /// and CAS inside the scope lock.
    pub fn admit(&self, actor: &Actor, intent: WriteIntent) -> Result<Admission, StoreError> {
        let evidence = self.evidence_for(actor, &intent)?;
        let caller = actor.caller();
        let expected = intent.expected.clone();
        let op_id = intent.op_id.clone();
        self.store.admit(AdmitRequest {
            op_id,
            caller,
            intent: intent.into_store_intent(evidence),
            expected,
        })
    }

    /// Park one create as a pending proposal. Use this only when no approver
    /// exists to settle the write; a replacement must go through `admit` so its
    /// target and authority are checked against current truth.
    pub fn propose(
        &self,
        actor: &Actor,
        intent: WriteIntent,
        reason: PendingReason,
    ) -> Result<Admission, StoreError> {
        let evidence = self.evidence_for(actor, &intent)?;
        let caller = actor.caller();
        let expected = intent.expected.clone();
        let op_id = intent.op_id.clone();
        self.store.propose(
            AdmitRequest {
                op_id,
                caller,
                intent: intent.into_store_intent(evidence),
                expected,
            },
            reason,
        )
    }

    pub fn update(
        &self,
        actor: &Actor,
        op_id: OpId,
        id: &MemoryId,
        patch: RecordPatch,
        expected: Cas,
    ) -> Result<Admission, StoreError> {
        self.store
            .update(op_id, actor.caller(), id, patch, expected)
    }

    pub fn deprecate(
        &self,
        actor: &Actor,
        op_id: OpId,
        id: &MemoryId,
        expected: Cas,
        reason: Reason,
    ) -> Result<Admission, StoreError> {
        self.store
            .deprecate(op_id, actor.caller(), id, expected, reason)
    }

    pub fn pin(
        &self,
        actor: &Actor,
        op_id: OpId,
        id: &MemoryId,
        expected: Cas,
    ) -> Result<Admission, StoreError> {
        let pinned_by = match actor {
            Actor::User { .. } => PinOwner::User,
            _ => PinOwner::Model,
        };
        self.store
            .pin(op_id, actor.caller(), id, pinned_by, expected)
    }

    pub fn unpin(
        &self,
        actor: &Actor,
        op_id: OpId,
        id: &MemoryId,
        expected: Cas,
    ) -> Result<Admission, StoreError> {
        self.store.unpin(op_id, actor.caller(), id, expected)
    }

    pub fn forget(
        &self,
        actor: &Actor,
        op_id: OpId,
        id: &MemoryId,
        expected: Cas,
    ) -> Result<ForgetOutcome, StoreError> {
        self.store.forget(op_id, actor.caller(), id, expected)
    }

    /// Accept or reject a pending record. Only a trusted user invocation may
    /// review; the operation id is derived from that invocation so a retry of
    /// the same user command replays instead of applying twice.
    pub fn review(
        &self,
        actor: &Actor,
        pending: &MemoryId,
        decision: ReviewDecision,
    ) -> Result<Admission, StoreError> {
        let Actor::User { invocation } = actor else {
            return Err(StoreError::ForbiddenActor(
                "only a trusted user invocation may accept or reject pending memory".into(),
            ));
        };
        let caller = Caller::UserReview {
            invocation: invocation.as_str().to_string(),
        };
        let action = match &decision {
            ReviewDecision::Accept => "accept",
            ReviewDecision::Reject { .. } => "reject",
        };
        let op_id = OpId::parse(format!("review:{}:{action}:{pending}", invocation.as_str()))
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        self.store.review(op_id, caller, pending, decision)
    }

    pub fn expire_due(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<MemoryId>, StoreError> {
        self.store.expire_due(now)
    }

    /// At most [`IDENTITY_CONTEXT_LIMIT`] trusted active identities, ordered by
    /// user pin, explicit user authority, then most recently updated. This is
    /// the input the extractor needs to reuse an existing id instead of
    /// guessing one.
    pub fn identity_context(&self, limit: usize) -> Result<Vec<IdentityRef>, StoreError> {
        let limit = if limit == 0 {
            IDENTITY_CONTEXT_LIMIT
        } else {
            limit.min(IDENTITY_CONTEXT_LIMIT)
        };
        let mut active = self.store.effective_active()?;
        active.sort_by_key(|entry| {
            (
                entry.record.frontmatter.pinned_by != Some(PinOwner::User),
                entry.effective_authority != MemoryAuthority::UserExplicit,
                Reverse(entry.record.frontmatter.updated_at),
            )
        });
        Ok(active
            .into_iter()
            .take(limit)
            .map(|entry| IdentityRef {
                id: entry.record.frontmatter.id.to_string(),
                title: entry.record.frontmatter.title.clone(),
                subject_key: entry.record.frontmatter.subject_key.clone(),
                subject: entry.record.frontmatter.subject.clone(),
                value: entry.record.frontmatter.value.clone(),
                revision: entry.record.frontmatter.revision,
                kind: entry.record.frontmatter.kind,
                authority: entry.effective_authority,
            })
            .collect())
    }

    /// Recover a prepared journal. Any read or write does this implicitly; the
    /// explicit entry exists for startup and operational diagnostics.
    pub fn recover(&self) -> Result<super::journal::RecoveryReport, StoreError> {
        self.store.recover()
    }

    /// Verify and stamp provenance. Caller-supplied evidence may carry
    /// observations, but it may not claim a user command or review unless the
    /// actor is that user, and a model may not claim it saw a user turn.
    fn evidence_for(
        &self,
        actor: &Actor,
        intent: &WriteIntent,
    ) -> Result<Vec<SourceRef>, StoreError> {
        let is_user = matches!(actor, Actor::User { .. });
        let user_claim = |source: &SourceRef| {
            matches!(
                source.kind,
                SourceRefKind::UserCommand | SourceRefKind::UserReview
            )
        };
        if !is_user && intent.evidence.iter().any(user_claim) {
            return Err(StoreError::Invalid(
                "evidence claims a user command/review without a user actor".into(),
            ));
        }
        if matches!(actor, Actor::Model { .. })
            && intent
                .evidence
                .iter()
                .any(|source| source.kind == SourceRefKind::UserTurn)
        {
            return Err(StoreError::Invalid(
                "a model write may not claim a user-turn evidence span".into(),
            ));
        }
        if matches!(actor, Actor::Runtime { .. })
            && intent.kind == MemoryKind::UserFact
            && !intent.evidence.iter().any(|source| {
                source.kind == SourceRefKind::UserTurn
                    && source
                        .evidence_span
                        .as_deref()
                        .is_some_and(|span| !span.trim().is_empty())
            })
        {
            return Err(StoreError::Invalid(
                "an automatically extracted user fact requires an exact user-turn evidence span"
                    .into(),
            ));
        }
        let mut evidence = intent.evidence.clone();
        // Fill the identity fields the runtime owns; a supplied reference may
        // describe an observation, but not who is observing it.
        for source in &mut evidence {
            match actor {
                Actor::Model {
                    session,
                    turn,
                    tool_call,
                } => {
                    source
                        .session_id
                        .get_or_insert_with(|| session.as_str().to_string());
                    source
                        .turn_id
                        .get_or_insert_with(|| turn.as_str().to_string());
                    source
                        .tool_call_id
                        .get_or_insert_with(|| tool_call.as_str().to_string());
                }
                Actor::Runtime { task } => {
                    source
                        .task_id
                        .get_or_insert_with(|| task.as_str().to_string());
                }
                Actor::User { .. } | Actor::Migration { .. } | Actor::External => {}
            }
        }
        // The actor's own channel reference is always kept, even when the
        // caller supplied an observation span: the span says what was seen,
        // this says which trusted operation is responsible for the write.
        evidence.push(actor.provenance());
        Ok(evidence)
    }
}
