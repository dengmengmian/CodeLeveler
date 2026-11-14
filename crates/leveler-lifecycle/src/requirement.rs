//! Structured requirement and scope contract.
//!
//! The user's instruction decides what a goal owes, but the runtime cannot read
//! arbitrary prose. A requirement is therefore a MODEL declaration anchored to a
//! verbatim span of the user's newest message: the runtime validates that the
//! span exists, that the requirement identity is real, that the transition is
//! legal over the recorded history, and that the event order is preserved. It
//! never re-derives meaning from the words.
//!
//! The quote proves the source exists, not that the interpretation is correct.
//! The original user message stays in the transcript and every accepted change
//! keeps its quote, so a later reader can audit what the user actually said.

use serde::{Deserialize, Serialize};

/// The most operations one declaration may carry.
pub const SCOPE_OPERATIONS_MAX: usize = 8;
/// The longest user span a declaration may quote.
pub const SCOPE_SOURCE_QUOTE_MAX_CHARS: usize = 8000;
/// The longest requirement text a declaration may state.
pub const REQUIREMENT_TEXT_MAX_CHARS: usize = 4000;
/// The longest path/identifier a requirement may target.
pub const REQUIREMENT_TARGET_MAX_CHARS: usize = 1000;
/// The longest free-form reason a declaration may attach.
pub const REQUIREMENT_REASON_MAX_CHARS: usize = 2000;
/// The most live requirements one goal may carry.
pub const SCOPE_REQUIREMENTS_MAX: usize = 200;

/// What kind of evidence can decide one requirement.
///
/// This is derived from the requirement's kind, never declared independently:
/// a model cannot register a weaker contract for a fact the runtime can check,
/// and it cannot claim a mechanical check for a model judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidencePolicy {
    /// The runtime must observe the declared fact in its execution record or
    /// the current workspace.
    Mechanical,
    /// A delivered model judgement is this requirement's completion contract.
    /// The runtime records the account but does not claim independent
    /// verification.
    ModelJudgment,
}

impl EvidencePolicy {
    fn strength(self) -> u8 {
        match self {
            Self::Mechanical => 2,
            Self::ModelJudgment => 1,
        }
    }
}

/// What kind of evidence can decide one requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementKind {
    /// A file/directory the user asked to exist, or whose content is the
    /// deliverable's location.
    Artifact,
    /// A file/directory the user asked to be removed.
    ArtifactAbsent,
    /// An executable path the user asked to run.
    Command,
    /// An explicitly requested deliverable whose check, if any, is registered separately.
    Named,
    /// A literal the user asked to appear in a named file.
    Content,
    /// The user explicitly required a test run to pass.
    Tests,
    /// The user forbade something.
    Forbidden,
    /// What an earlier phase already shipped, stated as context.
    Baseline,
    /// A step the user asked to perform before delivering.
    Preparation,
    /// A quality bar or constraint with no mechanical check.
    Constraint,
}

impl RequirementKind {
    /// Every requirement kind, in the order the model-facing schemas list it.
    pub const ALL: [Self; 10] = [
        Self::Artifact,
        Self::ArtifactAbsent,
        Self::Command,
        Self::Named,
        Self::Content,
        Self::Tests,
        Self::Forbidden,
        Self::Baseline,
        Self::Preparation,
        Self::Constraint,
    ];

    /// Whether this kind is a deliverable the runtime can hold the model to.
    pub fn is_deliverable(self) -> bool {
        !matches!(
            self,
            Self::Forbidden | Self::Baseline | Self::Preparation | Self::Constraint
        )
    }

    /// Human-readable class for tool results and diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Artifact => "artifact",
            Self::ArtifactAbsent => "absent-artifact",
            Self::Command => "command",
            Self::Named => "deliverable",
            Self::Content => "content",
            Self::Tests => "tests",
            Self::Forbidden => "forbidden",
            Self::Baseline => "baseline",
            Self::Preparation => "preparation",
            Self::Constraint => "constraint",
        }
    }

    /// The persisted spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Artifact => "artifact",
            Self::ArtifactAbsent => "artifact_absent",
            Self::Command => "command",
            Self::Named => "named",
            Self::Content => "content",
            Self::Tests => "tests",
            Self::Forbidden => "forbidden",
            Self::Baseline => "baseline",
            Self::Preparation => "preparation",
            Self::Constraint => "constraint",
        }
    }

    /// Parse a persisted kind. Unknown values are refused: a row the runtime
    /// cannot interpret must not silently become a different obligation.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "artifact" => Some(Self::Artifact),
            "artifact_absent" => Some(Self::ArtifactAbsent),
            "command" => Some(Self::Command),
            "named" => Some(Self::Named),
            "content" => Some(Self::Content),
            "tests" => Some(Self::Tests),
            "forbidden" => Some(Self::Forbidden),
            "baseline" => Some(Self::Baseline),
            "preparation" => Some(Self::Preparation),
            "constraint" => Some(Self::Constraint),
            _ => None,
        }
    }

    /// What `target` must hold for this kind, or `None` when the kind has no
    /// mechanical anchor. This is the single source for both validation and
    /// the model-facing schema: a kind that returns `Some` is exactly a kind
    /// the runtime refuses without `target`.
    pub fn target_purpose(self) -> Option<&'static str> {
        match self {
            Self::Artifact => Some("the workspace-relative path or bare file name the user named"),
            Self::ArtifactAbsent => {
                Some("the workspace-relative path or bare file name that must be gone")
            }
            Self::Command => Some("the exact command line"),
            Self::Content => Some("the file that must contain the literal `text`"),
            Self::Tests => Some("the exact test command line"),
            Self::Named
            | Self::Forbidden
            | Self::Baseline
            | Self::Preparation
            | Self::Constraint => None,
        }
    }

    /// Whether this kind needs a named target: the file for `artifact`,
    /// `artifact_absent` and `content`, or the exact command for `command` and
    /// `tests`. A tests requirement declares the command whose run is its
    /// evidence, so custom test commands need no recognised runner name.
    pub fn requires_target(self) -> bool {
        self.target_purpose().is_some()
    }

    /// The evidence this kind's completion contract requires.
    ///
    /// `None` means the kind is not owed at all (a prohibition, a baseline, a
    /// preparation step or a quality constraint), so there is no completion
    /// contract to assess.
    pub fn evidence_policy(self) -> Option<EvidencePolicy> {
        match self {
            Self::Artifact | Self::ArtifactAbsent | Self::Command | Self::Content | Self::Tests => {
                Some(EvidencePolicy::Mechanical)
            }
            Self::Named => Some(EvidencePolicy::ModelJudgment),
            Self::Forbidden | Self::Baseline | Self::Preparation | Self::Constraint => None,
        }
    }

    /// Ordering for change validation: a change may strengthen this contract
    /// but never silently weaken it.
    pub(crate) fn evidence_strength(self) -> u8 {
        self.evidence_policy().map_or(0, |policy| policy.strength())
    }
}

/// Where one requirement currently stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequirementState {
    Active,
    Waived,
    Superseded,
}

impl RequirementState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Waived => "waived",
            Self::Superseded => "superseded",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "waived" => Some(Self::Waived),
            "superseded" => Some(Self::Superseded),
            _ => None,
        }
    }
}

/// The identity of an acceptance check registered by the domain adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementVerification {
    /// A versioned fingerprint of the exact invocation. Raw arguments are not stored here.
    pub fingerprint: String,
    /// The host-issued revision identity of this registration.
    pub registered_by: String,
}

/// One requirement as it currently stands after folding the event history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementItem {
    /// Stable runtime identity (`R1`, `R2`, ...).
    pub id: String,
    pub kind: RequirementKind,
    /// The requirement as the model stated it.
    pub text: String,
    /// The path for an artifact, the file a `content` literal belongs to, or
    /// the exact command a `command` requirement names.
    pub target: Option<String>,
    /// The verbatim user span this requirement is anchored to.
    pub source_quote: String,
    /// The acceptance check the model associated with it, when one exists.
    pub check: Option<String>,
    pub verification: Option<RequirementVerification>,
    pub state: RequirementState,
    /// For a superseded item, the requirement that replaced it.
    pub superseded_by: Option<String>,
}

impl RequirementItem {
    /// Whether the completion gate still owes this item.
    pub fn is_owed(&self) -> bool {
        self.state == RequirementState::Active && self.kind.is_deliverable()
    }
}

/// One accepted scope operation, as recorded in durable history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRequirementEvent {
    pub operation: ScopeOperationKind,
    /// The requirement the operation produced or targeted.
    pub requirement_id: String,
    /// For `replace`, the requirement that was retired.
    pub supersedes: Option<String>,
    pub kind: Option<RequirementKind>,
    pub text: Option<String>,
    pub target: Option<String>,
    pub source_quote: String,
    pub check: Option<String>,
    pub verification: Option<RequirementVerification>,
    pub reason: Option<String>,
}

/// The operation names as recorded and replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeOperationKind {
    Add,
    Amend,
    Replace,
    Waive,
    Restore,
}

impl ScopeOperationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Amend => "amend",
            Self::Replace => "replace",
            Self::Waive => "waive",
            Self::Restore => "restore",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "add" => Some(Self::Add),
            "amend" => Some(Self::Amend),
            "replace" => Some(Self::Replace),
            "waive" => Some(Self::Waive),
            "restore" => Some(Self::Restore),
            _ => None,
        }
    }
}

/// One operation in a scope declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ScopeOperation {
    Add {
        kind: RequirementKind,
        text: String,
        #[serde(default)]
        target: Option<String>,
        source_quote: String,
        #[serde(default)]
        check: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification: Option<RequirementVerification>,
        #[serde(default)]
        reason: Option<String>,
    },
    Amend {
        requirement_id: String,
        #[serde(default)]
        kind: Option<RequirementKind>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        target: Option<String>,
        source_quote: String,
        #[serde(default)]
        check: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification: Option<RequirementVerification>,
        #[serde(default)]
        reason: Option<String>,
    },
    Replace {
        requirement_id: String,
        kind: RequirementKind,
        text: String,
        #[serde(default)]
        target: Option<String>,
        source_quote: String,
        #[serde(default)]
        check: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification: Option<RequirementVerification>,
        #[serde(default)]
        reason: Option<String>,
    },
    Waive {
        requirement_id: String,
        source_quote: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Restore {
        requirement_id: String,
        source_quote: String,
        #[serde(default)]
        reason: Option<String>,
    },
}

impl ScopeOperation {
    /// The verbatim user span this operation is anchored to.
    pub fn source_quote(&self) -> &str {
        match self {
            Self::Add { source_quote, .. }
            | Self::Amend { source_quote, .. }
            | Self::Replace { source_quote, .. }
            | Self::Waive { source_quote, .. }
            | Self::Restore { source_quote, .. } => source_quote,
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Add { reason, .. }
            | Self::Amend { reason, .. }
            | Self::Replace { reason, .. }
            | Self::Waive { reason, .. }
            | Self::Restore { reason, .. } => reason.as_deref(),
        }
    }
}

/// One model scope declaration, applied in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeDeclaration {
    pub operations: Vec<ScopeOperation>,
}

/// One requirement declared when the task is committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementSeed {
    pub kind: RequirementKind,
    pub text: String,
    pub target: Option<String>,
    pub source_quote: String,
    pub check: Option<String>,
    pub verification: Option<RequirementVerification>,
}

/// The result of accepting a declaration: the new current set and the ordered
/// events that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeProjection {
    pub items: Vec<RequirementItem>,
    pub events: Vec<AppliedRequirementEvent>,
}

/// A declaration the runtime cannot accept.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    #[error("a scope declaration needs at least one operation")]
    EmptyOperations,
    #[error("a scope declaration may carry at most {SCOPE_OPERATIONS_MAX} operations")]
    TooManyOperations,
    #[error("the scope operation quotes no user text; quote the user's own words verbatim")]
    EmptySourceQuote,
    #[error("the quoted user span is {length} characters; the limit is {max}")]
    SourceQuoteTooLong { length: usize, max: usize },
    #[error("the requirement text is empty")]
    EmptyText,
    #[error("the requirement text is {length} characters; the limit is {max}")]
    TextTooLong { length: usize, max: usize },
    #[error("the `{kind}` requirement needs `target`: {purpose}")]
    TargetRequired {
        kind: &'static str,
        purpose: &'static str,
    },
    #[error("the target is {length} characters; the limit is {max}")]
    TargetTooLong { length: usize, max: usize },
    #[error("the reason is {length} characters; the limit is {max}")]
    ReasonTooLong { length: usize, max: usize },
    #[error("requirement `{id}` does not exist in this goal")]
    UnknownRequirement { id: String },
    #[error("requirement `{id}` is not active; its current state is `{state}`")]
    RequirementNotActive { id: String, state: &'static str },
    #[error("requirement `{id}` is not waived; only a waived requirement can be restored")]
    RequirementNotWaived { id: String },
    #[error("the goal already carries {max} requirements; waive or replace one first")]
    TooManyRequirements { max: usize },
    #[error("registered verification is only supported for `named` requirements")]
    VerificationRequiresNamed,
    #[error("requirement `{id}` has registered verification; keep it or register its replacement")]
    CannotDropVerification { id: String },
    #[error("requirement `{id}` changed its outcome; register its acceptance check again")]
    VerificationNeedsRegistration { id: String },
    #[error(
        "requirement `{id}` cannot weaken its completion contract from `{from}` to `{to}` through \
         `amend`; `replace` it with a new requirement anchored to the user's newest words"
    )]
    EvidencePolicyDowngrade {
        id: String,
        from: &'static str,
        to: &'static str,
    },
    #[error("the recorded scope history is corrupt: {0}")]
    CorruptHistory(String),
}

/// Validate the declaration's shape and bounds. Identity and transition checks
/// run during application, against the current set.
pub fn validate_declaration(declaration: &ScopeDeclaration) -> Result<(), ScopeError> {
    if declaration.operations.is_empty() {
        return Err(ScopeError::EmptyOperations);
    }
    if declaration.operations.len() > SCOPE_OPERATIONS_MAX {
        return Err(ScopeError::TooManyOperations);
    }
    for operation in &declaration.operations {
        let quote = operation.source_quote().trim();
        if quote.is_empty() {
            return Err(ScopeError::EmptySourceQuote);
        }
        let length = quote.chars().count();
        if length > SCOPE_SOURCE_QUOTE_MAX_CHARS {
            return Err(ScopeError::SourceQuoteTooLong {
                length,
                max: SCOPE_SOURCE_QUOTE_MAX_CHARS,
            });
        }
        if let Some(reason) = operation.reason() {
            let length = reason.chars().count();
            if length > REQUIREMENT_REASON_MAX_CHARS {
                return Err(ScopeError::ReasonTooLong {
                    length,
                    max: REQUIREMENT_REASON_MAX_CHARS,
                });
            }
        }
        match operation {
            ScopeOperation::Add {
                kind,
                text,
                target,
                verification,
                ..
            }
            | ScopeOperation::Replace {
                kind,
                text,
                target,
                verification,
                ..
            } => {
                validate_text(text)?;
                validate_target(*kind, target.as_deref())?;
                validate_verification(*kind, verification.as_ref())?;
            }
            ScopeOperation::Amend {
                requirement_id,
                kind,
                text,
                target,
                ..
            } => {
                if requirement_id.trim().is_empty() {
                    return Err(ScopeError::UnknownRequirement {
                        id: requirement_id.clone(),
                    });
                }
                if let Some(text) = text {
                    validate_text(text)?;
                }
                if let Some(kind) = kind {
                    validate_target(*kind, target.as_deref())?;
                }
            }
            ScopeOperation::Waive { requirement_id, .. }
            | ScopeOperation::Restore { requirement_id, .. } => {
                if requirement_id.trim().is_empty() {
                    return Err(ScopeError::UnknownRequirement {
                        id: requirement_id.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn validate_verification(
    kind: RequirementKind,
    verification: Option<&RequirementVerification>,
) -> Result<(), ScopeError> {
    if verification.is_some() && kind != RequirementKind::Named {
        return Err(ScopeError::VerificationRequiresNamed);
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<(), ScopeError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ScopeError::EmptyText);
    }
    let length = text.chars().count();
    if length > REQUIREMENT_TEXT_MAX_CHARS {
        return Err(ScopeError::TextTooLong {
            length,
            max: REQUIREMENT_TEXT_MAX_CHARS,
        });
    }
    Ok(())
}

fn validate_target(kind: RequirementKind, target: Option<&str>) -> Result<(), ScopeError> {
    match target.map(str::trim).filter(|value| !value.is_empty()) {
        Some(target) => {
            let length = target.chars().count();
            if length > REQUIREMENT_TARGET_MAX_CHARS {
                return Err(ScopeError::TargetTooLong {
                    length,
                    max: REQUIREMENT_TARGET_MAX_CHARS,
                });
            }
        }
        None => {
            if let Some(purpose) = kind.target_purpose() {
                return Err(ScopeError::TargetRequired {
                    kind: kind.as_str(),
                    purpose,
                });
            }
        }
    }
    Ok(())
}

/// The next stable requirement identity. Superseded items keep their id
/// reserved, so a later requirement never reuses one.
pub fn next_requirement_id(current: &[RequirementItem]) -> String {
    let highest = current
        .iter()
        .filter_map(|item| item.id.strip_prefix('R'))
        .filter_map(|number| number.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("R{}", highest + 1)
}

/// Apply one declaration in order over the current set.
pub fn apply_scope_declaration(
    current: &[RequirementItem],
    declaration: &ScopeDeclaration,
) -> Result<ScopeProjection, ScopeError> {
    validate_declaration(declaration)?;
    let mut items = current.to_vec();
    let mut events = Vec::with_capacity(declaration.operations.len());
    for operation in &declaration.operations {
        apply_operation(&mut items, operation, &mut events)?;
    }
    Ok(ScopeProjection { items, events })
}

fn apply_operation(
    items: &mut Vec<RequirementItem>,
    operation: &ScopeOperation,
    events: &mut Vec<AppliedRequirementEvent>,
) -> Result<(), ScopeError> {
    match operation {
        ScopeOperation::Add {
            kind,
            text,
            target,
            source_quote,
            check,
            verification,
            reason,
        } => {
            if items
                .iter()
                .filter(|item| item.state != RequirementState::Superseded)
                .count()
                >= SCOPE_REQUIREMENTS_MAX
            {
                return Err(ScopeError::TooManyRequirements {
                    max: SCOPE_REQUIREMENTS_MAX,
                });
            }
            let id = next_requirement_id(items);
            items.push(RequirementItem {
                id: id.clone(),
                kind: *kind,
                text: text.trim().to_string(),
                target: normalized(target),
                source_quote: source_quote.trim().to_string(),
                check: normalized(check),
                verification: verification.clone(),
                state: RequirementState::Active,
                superseded_by: None,
            });
            events.push(AppliedRequirementEvent {
                operation: ScopeOperationKind::Add,
                requirement_id: id,
                supersedes: None,
                kind: Some(*kind),
                text: Some(text.trim().to_string()),
                target: normalized(target),
                source_quote: source_quote.trim().to_string(),
                check: normalized(check),
                verification: verification.clone(),
                reason: normalized(reason),
            });
        }
        ScopeOperation::Amend {
            requirement_id,
            kind,
            text,
            target,
            source_quote,
            check,
            verification,
            reason,
        } => {
            let index = active_index(items, requirement_id)?;
            let next_kind = kind.unwrap_or(items[index].kind);
            // A registered check belongs to the named judgement it was
            // registered for. A stronger mechanical kind owns its own
            // evidence, so it does not inherit that registration.
            let keeps_registered_check = next_kind == RequirementKind::Named;
            validate_verification(
                next_kind,
                verification.as_ref().or_else(|| {
                    keeps_registered_check
                        .then_some(items[index].verification.as_ref())
                        .flatten()
                }),
            )?;
            if let Some(next) = kind
                && next.evidence_strength() < items[index].kind.evidence_strength()
            {
                // A requirement the runtime can check cannot turn into model
                // judgement in place. Retiring it through `replace` is an
                // explicit, separately recorded change with a new identity.
                return Err(ScopeError::EvidencePolicyDowngrade {
                    id: requirement_id.clone(),
                    from: items[index].kind.label(),
                    to: next.label(),
                });
            }
            if items[index].verification.is_some()
                && verification.is_none()
                && keeps_registered_check
                && text
                    .as_deref()
                    .is_some_and(|text| text.trim() != items[index].text)
            {
                return Err(ScopeError::VerificationNeedsRegistration {
                    id: requirement_id.clone(),
                });
            }
            if let Some(kind) = kind {
                items[index].kind = *kind;
                if *kind != RequirementKind::Named {
                    items[index].verification = None;
                }
            }
            if let Some(text) = text {
                items[index].text = text.trim().to_string();
            }
            if target.is_some() {
                items[index].target = normalized(target);
            }
            if check.is_some() {
                items[index].check = normalized(check);
            }
            if let Some(verification) = verification {
                items[index].verification = Some(verification.clone());
            }
            items[index].source_quote = source_quote.trim().to_string();
            events.push(AppliedRequirementEvent {
                operation: ScopeOperationKind::Amend,
                requirement_id: requirement_id.clone(),
                supersedes: None,
                kind: *kind,
                text: text.as_ref().map(|text| text.trim().to_string()),
                target: normalized(target),
                source_quote: source_quote.trim().to_string(),
                check: normalized(check),
                verification: verification.clone(),
                reason: normalized(reason),
            });
        }
        ScopeOperation::Replace {
            requirement_id,
            kind,
            text,
            target,
            source_quote,
            check,
            verification,
            reason,
        } => {
            let index = active_index(items, requirement_id)?;
            if items[index].verification.is_some()
                && verification.is_none()
                && kind.evidence_strength() <= items[index].kind.evidence_strength()
            {
                return Err(ScopeError::CannotDropVerification {
                    id: requirement_id.clone(),
                });
            }
            if items
                .iter()
                .filter(|item| item.state != RequirementState::Superseded)
                .count()
                >= SCOPE_REQUIREMENTS_MAX
            {
                return Err(ScopeError::TooManyRequirements {
                    max: SCOPE_REQUIREMENTS_MAX,
                });
            }
            let new_id = next_requirement_id(items);
            items[index].state = RequirementState::Superseded;
            items[index].superseded_by = Some(new_id.clone());
            items.push(RequirementItem {
                id: new_id.clone(),
                kind: *kind,
                text: text.trim().to_string(),
                target: normalized(target),
                source_quote: source_quote.trim().to_string(),
                check: normalized(check),
                verification: verification.clone(),
                state: RequirementState::Active,
                superseded_by: None,
            });
            events.push(AppliedRequirementEvent {
                operation: ScopeOperationKind::Replace,
                requirement_id: new_id,
                supersedes: Some(requirement_id.clone()),
                kind: Some(*kind),
                text: Some(text.trim().to_string()),
                target: normalized(target),
                source_quote: source_quote.trim().to_string(),
                check: normalized(check),
                verification: verification.clone(),
                reason: normalized(reason),
            });
        }
        ScopeOperation::Waive {
            requirement_id,
            source_quote,
            reason,
        } => {
            let index = active_index(items, requirement_id)?;
            items[index].state = RequirementState::Waived;
            events.push(AppliedRequirementEvent {
                operation: ScopeOperationKind::Waive,
                requirement_id: requirement_id.clone(),
                supersedes: None,
                kind: None,
                text: None,
                target: None,
                source_quote: source_quote.trim().to_string(),
                check: None,
                verification: None,
                reason: normalized(reason),
            });
        }
        ScopeOperation::Restore {
            requirement_id,
            source_quote,
            reason,
        } => {
            let Some(index) = items.iter().position(|item| item.id == *requirement_id) else {
                return Err(ScopeError::UnknownRequirement {
                    id: requirement_id.clone(),
                });
            };
            if items[index].state != RequirementState::Waived {
                return Err(ScopeError::RequirementNotWaived {
                    id: requirement_id.clone(),
                });
            }
            items[index].state = RequirementState::Active;
            events.push(AppliedRequirementEvent {
                operation: ScopeOperationKind::Restore,
                requirement_id: requirement_id.clone(),
                supersedes: None,
                kind: None,
                text: None,
                target: None,
                source_quote: source_quote.trim().to_string(),
                check: None,
                verification: None,
                reason: normalized(reason),
            });
        }
    }
    Ok(())
}

fn active_index(items: &[RequirementItem], id: &str) -> Result<usize, ScopeError> {
    let Some(index) = items.iter().position(|item| item.id == id) else {
        return Err(ScopeError::UnknownRequirement { id: id.to_string() });
    };
    if items[index].state != RequirementState::Active {
        return Err(ScopeError::RequirementNotActive {
            id: id.to_string(),
            state: items[index].state.as_str(),
        });
    }
    Ok(index)
}

fn normalized(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Rebuild the current set by folding accepted events in order.
pub fn fold_scope_events(
    events: &[AppliedRequirementEvent],
) -> Result<Vec<RequirementItem>, ScopeError> {
    let mut items: Vec<RequirementItem> = Vec::new();
    for event in events {
        match event.operation {
            ScopeOperationKind::Add => {
                let kind = event.kind.ok_or_else(|| {
                    ScopeError::CorruptHistory(format!(
                        "add event for {} carries no kind",
                        event.requirement_id
                    ))
                })?;
                let text = event.text.clone().ok_or_else(|| {
                    ScopeError::CorruptHistory(format!(
                        "add event for {} carries no text",
                        event.requirement_id
                    ))
                })?;
                if items.iter().any(|item| item.id == event.requirement_id) {
                    return Err(ScopeError::CorruptHistory(format!(
                        "requirement {} was added twice",
                        event.requirement_id
                    )));
                }
                items.push(RequirementItem {
                    id: event.requirement_id.clone(),
                    kind,
                    text,
                    target: event.target.clone(),
                    source_quote: event.source_quote.clone(),
                    check: event.check.clone(),
                    verification: event.verification.clone(),
                    state: RequirementState::Active,
                    superseded_by: None,
                });
            }
            ScopeOperationKind::Amend => {
                let Some(index) = items
                    .iter()
                    .position(|item| item.id == event.requirement_id)
                else {
                    return Err(ScopeError::CorruptHistory(format!(
                        "amend targets unknown requirement {}",
                        event.requirement_id
                    )));
                };
                if items[index].state != RequirementState::Active {
                    return Err(ScopeError::CorruptHistory(format!(
                        "amend targets {} which is {}",
                        event.requirement_id,
                        items[index].state.as_str()
                    )));
                }
                if let Some(kind) = event.kind {
                    items[index].kind = kind;
                    if kind != RequirementKind::Named {
                        // The amend moved the requirement to a stronger
                        // mechanical kind; the named registration it used to
                        // carry is not part of that contract.
                        items[index].verification = None;
                    }
                }
                if let Some(text) = &event.text {
                    items[index].text = text.clone();
                }
                if event.target.is_some() {
                    items[index].target = event.target.clone();
                }
                if event.check.is_some() {
                    items[index].check = event.check.clone();
                }
                if let Some(verification) = &event.verification {
                    items[index].verification = Some(verification.clone());
                }
                items[index].source_quote = event.source_quote.clone();
            }
            ScopeOperationKind::Replace => {
                let retired = event.supersedes.as_deref().ok_or_else(|| {
                    ScopeError::CorruptHistory(format!(
                        "replace event for {} names no retired requirement",
                        event.requirement_id
                    ))
                })?;
                let Some(index) = items.iter().position(|item| item.id == retired) else {
                    return Err(ScopeError::CorruptHistory(format!(
                        "replace retires unknown requirement {retired}"
                    )));
                };
                if items[index].state != RequirementState::Active {
                    return Err(ScopeError::CorruptHistory(format!(
                        "replace retires {retired} which is {}",
                        items[index].state.as_str()
                    )));
                }
                let kind = event.kind.ok_or_else(|| {
                    ScopeError::CorruptHistory(format!(
                        "replace event for {} carries no kind",
                        event.requirement_id
                    ))
                })?;
                let text = event.text.clone().ok_or_else(|| {
                    ScopeError::CorruptHistory(format!(
                        "replace event for {} carries no text",
                        event.requirement_id
                    ))
                })?;
                items[index].state = RequirementState::Superseded;
                items[index].superseded_by = Some(event.requirement_id.clone());
                items.push(RequirementItem {
                    id: event.requirement_id.clone(),
                    kind,
                    text,
                    target: event.target.clone(),
                    source_quote: event.source_quote.clone(),
                    check: event.check.clone(),
                    verification: event.verification.clone(),
                    state: RequirementState::Active,
                    superseded_by: None,
                });
            }
            ScopeOperationKind::Waive => {
                let Some(index) = items
                    .iter()
                    .position(|item| item.id == event.requirement_id)
                else {
                    return Err(ScopeError::CorruptHistory(format!(
                        "waive targets unknown requirement {}",
                        event.requirement_id
                    )));
                };
                if items[index].state != RequirementState::Active {
                    return Err(ScopeError::CorruptHistory(format!(
                        "waive targets {} which is {}",
                        event.requirement_id,
                        items[index].state.as_str()
                    )));
                }
                items[index].state = RequirementState::Waived;
            }
            ScopeOperationKind::Restore => {
                let Some(index) = items
                    .iter()
                    .position(|item| item.id == event.requirement_id)
                else {
                    return Err(ScopeError::CorruptHistory(format!(
                        "restore targets unknown requirement {}",
                        event.requirement_id
                    )));
                };
                if items[index].state != RequirementState::Waived {
                    return Err(ScopeError::CorruptHistory(format!(
                        "restore targets {} which is {}",
                        event.requirement_id,
                        items[index].state.as_str()
                    )));
                }
                items[index].state = RequirementState::Active;
            }
        }
    }
    Ok(items)
}

/// Build the initial requirement set declared at commit time.
///
/// The caller has already checked each quote against the committed objective.
/// IDs are assigned in seed order.
pub fn initial_scope(seeds: &[RequirementSeed]) -> Result<ScopeProjection, ScopeError> {
    if seeds.len() > SCOPE_REQUIREMENTS_MAX {
        return Err(ScopeError::TooManyRequirements {
            max: SCOPE_REQUIREMENTS_MAX,
        });
    }
    let mut items: Vec<RequirementItem> = Vec::with_capacity(seeds.len());
    let mut events = Vec::with_capacity(seeds.len());
    for seed in seeds {
        let text = seed.text.trim();
        validate_text(text)?;
        let quote = seed.source_quote.trim();
        if quote.is_empty() {
            return Err(ScopeError::EmptySourceQuote);
        }
        let quote_length = quote.chars().count();
        if quote_length > SCOPE_SOURCE_QUOTE_MAX_CHARS {
            return Err(ScopeError::SourceQuoteTooLong {
                length: quote_length,
                max: SCOPE_SOURCE_QUOTE_MAX_CHARS,
            });
        }
        validate_target(seed.kind, seed.target.as_deref())?;
        validate_verification(seed.kind, seed.verification.as_ref())?;
        let id = next_requirement_id(&items);
        items.push(RequirementItem {
            id: id.clone(),
            kind: seed.kind,
            text: text.to_string(),
            target: normalized(&seed.target),
            source_quote: quote.to_string(),
            check: normalized(&seed.check),
            verification: seed.verification.clone(),
            state: RequirementState::Active,
            superseded_by: None,
        });
        events.push(AppliedRequirementEvent {
            operation: ScopeOperationKind::Add,
            requirement_id: id,
            supersedes: None,
            kind: Some(seed.kind),
            text: Some(text.to_string()),
            target: normalized(&seed.target),
            source_quote: quote.to_string(),
            check: normalized(&seed.check),
            verification: seed.verification.clone(),
            reason: None,
        });
    }
    Ok(ScopeProjection { items, events })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: RequirementKind, text: &str) -> RequirementItem {
        RequirementItem {
            id: id.to_string(),
            kind,
            text: text.to_string(),
            target: None,
            source_quote: text.to_string(),
            check: None,
            verification: None,
            state: RequirementState::Active,
            superseded_by: None,
        }
    }

    fn add(text: &str, quote: &str) -> ScopeOperation {
        ScopeOperation::Add {
            kind: RequirementKind::Named,
            text: text.to_string(),
            target: None,
            source_quote: quote.to_string(),
            check: None,
            verification: None,
            reason: None,
        }
    }

    fn declaration(operations: Vec<ScopeOperation>) -> ScopeDeclaration {
        ScopeDeclaration { operations }
    }

    fn ids(items: &[RequirementItem]) -> Vec<(String, &'static str)> {
        items
            .iter()
            .map(|item| (item.id.clone(), item.state.as_str()))
            .collect()
    }

    #[test]
    fn adding_requirements_assigns_stable_ids_in_order() {
        let projected = apply_scope_declaration(
            &[item("R1", RequirementKind::Named, "existing")],
            &declaration(vec![
                add("first new", "add first new"),
                add("second new", "add second new"),
            ]),
        )
        .expect("declaration applies");
        assert_eq!(
            ids(&projected.items),
            vec![
                ("R1".to_string(), "active"),
                ("R2".to_string(), "active"),
                ("R3".to_string(), "active"),
            ]
        );
        assert_eq!(projected.events.len(), 2);
        assert_eq!(projected.events[0].requirement_id, "R2");
        assert_eq!(projected.events[1].requirement_id, "R3");
    }

    #[test]
    fn a_waiver_keeps_identity_and_restore_revives_it() {
        let waived = apply_scope_declaration(
            &[item("R1", RequirementKind::Named, "docs")],
            &declaration(vec![ScopeOperation::Waive {
                requirement_id: "R1".to_string(),
                source_quote: "不再需要文档".to_string(),
                reason: None,
            }]),
        )
        .expect("waive applies");
        assert_eq!(waived.items[0].state, RequirementState::Waived);
        assert!(!waived.items[0].is_owed());

        let restored = apply_scope_declaration(
            &waived.items,
            &declaration(vec![ScopeOperation::Restore {
                requirement_id: "R1".to_string(),
                source_quote: "文档还是要的".to_string(),
                reason: None,
            }]),
        )
        .expect("restore applies");
        assert_eq!(restored.items[0].state, RequirementState::Active);
        assert!(restored.items[0].is_owed());
    }

    #[test]
    fn replace_retires_the_old_identity_and_never_reuses_it() {
        let projected = apply_scope_declaration(
            &[item("R1", RequirementKind::Named, "dark theme")],
            &declaration(vec![ScopeOperation::Replace {
                requirement_id: "R1".to_string(),
                kind: RequirementKind::Named,
                text: "light theme".to_string(),
                target: None,
                source_quote: "把深色主题换成浅色主题".to_string(),
                check: None,
                verification: None,
                reason: None,
            }]),
        )
        .expect("replace applies");
        assert_eq!(
            ids(&projected.items),
            vec![
                ("R1".to_string(), "superseded"),
                ("R2".to_string(), "active"),
            ]
        );
        assert_eq!(projected.items[0].superseded_by.as_deref(), Some("R2"));
        assert!(!projected.items[0].is_owed());
        assert_eq!(projected.items[1].text, "light theme");

        let next =
            apply_scope_declaration(&projected.items, &declaration(vec![add("next", "next")]))
                .expect("next add applies");
        assert_eq!(next.events[0].requirement_id, "R3");
    }

    #[test]
    fn a_waived_requirement_cannot_be_amended_or_replaced() {
        let waived = item("R1", RequirementKind::Named, "docs");
        let mut waived = waived;
        waived.state = RequirementState::Waived;
        let error = apply_scope_declaration(
            &[waived],
            &declaration(vec![ScopeOperation::Amend {
                requirement_id: "R1".to_string(),
                kind: None,
                text: Some("new docs".to_string()),
                target: None,
                source_quote: "update docs".to_string(),
                check: None,
                verification: None,
                reason: None,
            }]),
        )
        .expect_err("amend of a waived requirement is refused");
        assert!(matches!(
            error,
            ScopeError::RequirementNotActive { state, .. } if state == "waived"
        ));
    }

    #[test]
    fn an_amend_cannot_weaken_a_mechanical_contract() {
        let mut tests = item("R1", RequirementKind::Tests, "the suite must pass");
        tests.target = Some("make check".to_string());
        let amend = |kind| {
            declaration(vec![ScopeOperation::Amend {
                requirement_id: "R1".to_string(),
                kind: Some(kind),
                text: None,
                target: None,
                source_quote: "just summarize it".to_string(),
                check: None,
                verification: None,
                reason: None,
            }])
        };
        let weakened = apply_scope_declaration(&[tests.clone()], &amend(RequirementKind::Named))
            .expect_err("a mechanical fact cannot become model judgement in place");
        assert!(
            matches!(
                weakened,
                ScopeError::EvidencePolicyDowngrade {
                    from: "tests",
                    to: "deliverable",
                    ..
                }
            ),
            "{weakened:?}"
        );
        let dropped = apply_scope_declaration(&[tests], &amend(RequirementKind::Constraint))
            .expect_err("an owed requirement cannot stop being owed in place");
        assert!(
            matches!(
                dropped,
                ScopeError::EvidencePolicyDowngrade {
                    to: "constraint",
                    ..
                }
            ),
            "{dropped:?}"
        );
    }

    #[test]
    fn an_amend_may_strengthen_a_model_judgement_contract() {
        let projected = apply_scope_declaration(
            &[item("R1", RequirementKind::Named, "the report")],
            &declaration(vec![ScopeOperation::Amend {
                requirement_id: "R1".to_string(),
                kind: Some(RequirementKind::Artifact),
                text: None,
                target: Some("report.md".to_string()),
                source_quote: "write report.md".to_string(),
                check: None,
                verification: None,
                reason: None,
            }]),
        )
        .expect("strengthening the contract is allowed");
        assert_eq!(projected.items[0].kind, RequirementKind::Artifact);
        assert_eq!(projected.items[0].target.as_deref(), Some("report.md"));
    }

    #[test]
    fn an_unknown_identity_is_refused_instead_of_created() {
        let error = apply_scope_declaration(
            &[],
            &declaration(vec![ScopeOperation::Waive {
                requirement_id: "R9".to_string(),
                source_quote: "waive it".to_string(),
                reason: None,
            }]),
        )
        .expect_err("unknown requirement");
        assert_eq!(
            error,
            ScopeError::UnknownRequirement {
                id: "R9".to_string()
            }
        );
    }

    #[test]
    fn a_content_requirement_must_name_its_file() {
        let error = apply_scope_declaration(
            &[],
            &declaration(vec![ScopeOperation::Add {
                kind: RequirementKind::Content,
                text: "Verified.".to_string(),
                target: None,
                source_quote: "append Verified.".to_string(),
                check: None,
                verification: None,
                reason: None,
            }]),
        )
        .expect_err("target required");
        assert!(matches!(error, ScopeError::TargetRequired { .. }));
    }

    /// A command requirement names the exact command in `target`; without it
    /// the completion gate has no action identity to compare against.
    #[test]
    fn a_command_requirement_must_name_the_command() {
        let error = apply_scope_declaration(
            &[],
            &declaration(vec![ScopeOperation::Add {
                kind: RequirementKind::Command,
                text: "run the verifier".to_string(),
                target: None,
                source_quote: "运行 sh verify.sh".to_string(),
                check: None,
                verification: None,
                reason: None,
            }]),
        )
        .expect_err("target required");
        assert_eq!(
            error,
            ScopeError::TargetRequired {
                kind: "command",
                purpose: "the exact command line"
            }
        );
    }

    #[test]
    fn a_tests_requirement_must_name_its_command() {
        let error = apply_scope_declaration(
            &[],
            &declaration(vec![ScopeOperation::Add {
                kind: RequirementKind::Tests,
                text: "the suite must pass".to_string(),
                target: None,
                source_quote: "run the suite".to_string(),
                check: None,
                verification: None,
                reason: None,
            }]),
        )
        .expect_err("target required");
        assert_eq!(
            error,
            ScopeError::TargetRequired {
                kind: "tests",
                purpose: "the exact test command line"
            }
        );
    }

    /// The target contract is one set: changing it here changes the model
    /// schema and the completion gate together.
    #[test]
    fn the_kinds_that_require_a_target_are_the_path_and_command_kinds() {
        let requiring: Vec<&str> = RequirementKind::ALL
            .iter()
            .filter(|kind| kind.requires_target())
            .map(|kind| kind.as_str())
            .collect();
        assert_eq!(
            requiring,
            ["artifact", "artifact_absent", "command", "content", "tests"]
        );
        for kind in RequirementKind::ALL {
            assert_eq!(
                kind.requires_target(),
                kind.target_purpose().is_some(),
                "`{}` must derive both answers from the same contract",
                kind.as_str()
            );
        }
    }

    #[test]
    fn operations_apply_in_order_within_one_declaration() {
        let projected = apply_scope_declaration(
            &[],
            &declaration(vec![
                add("first", "first"),
                ScopeOperation::Amend {
                    requirement_id: "R1".to_string(),
                    kind: None,
                    text: Some("first amended".to_string()),
                    target: None,
                    source_quote: "no, amend first".to_string(),
                    check: None,
                    verification: None,
                    reason: None,
                },
                ScopeOperation::Waive {
                    requirement_id: "R1".to_string(),
                    source_quote: "drop it".to_string(),
                    reason: None,
                },
            ]),
        )
        .expect("ordered operations apply");
        assert_eq!(projected.items[0].text, "first amended");
        assert_eq!(projected.items[0].state, RequirementState::Waived);
        let operations = projected
            .events
            .iter()
            .map(|event| event.operation.as_str())
            .collect::<Vec<_>>();
        assert_eq!(operations, vec!["add", "amend", "waive"]);
    }

    /// `replace` is the explicit, separately recorded way to retire a
    /// mechanical obligation. The retired requirement keeps its kind, target
    /// and identity in the history; the new requirement has its own identity.
    /// Nothing is silently deleted.
    #[test]
    fn replacing_a_mechanical_contract_retires_it_as_a_recorded_event() {
        let seeded = initial_scope(&[RequirementSeed {
            kind: RequirementKind::Tests,
            text: "the suite must pass".to_string(),
            target: Some("make regression".to_string()),
            source_quote: "run the suite".to_string(),
            check: None,
            verification: None,
        }])
        .expect("seed");
        let projected = apply_scope_declaration(
            &seeded.items,
            &declaration(vec![ScopeOperation::Replace {
                requirement_id: "R1".to_string(),
                kind: RequirementKind::Named,
                text: "summarize the suite result".to_string(),
                target: None,
                source_quote: "只要总结测试结果".to_string(),
                check: None,
                verification: None,
                reason: Some("the user asked for a summary instead".to_string()),
            }]),
        )
        .expect("an explicitly anchored replacement applies");
        assert_eq!(projected.items[0].state, RequirementState::Superseded);
        assert_eq!(projected.items[0].kind, RequirementKind::Tests);
        assert_eq!(
            projected.items[0].target.as_deref(),
            Some("make regression")
        );
        assert_eq!(projected.items[0].superseded_by.as_deref(), Some("R2"));
        assert_eq!(projected.items[1].state, RequirementState::Active);
        assert_eq!(projected.items[1].kind, RequirementKind::Named);
        assert_eq!(projected.events.len(), 1);
        assert_eq!(projected.events[0].operation, ScopeOperationKind::Replace);
        assert_eq!(projected.events[0].supersedes.as_deref(), Some("R1"));
        assert_eq!(
            projected.events[0].source_quote,
            "只要总结测试结果".to_string()
        );
        let history = [seeded.events, projected.events].concat();
        assert_eq!(fold_scope_events(&history).unwrap(), projected.items);
    }

    /// A registered acceptance check is part of the requirement's contract.
    /// `replace` cannot drop it: the replacement must register its own check.
    #[test]
    fn a_registered_check_cannot_be_dropped_by_replace() {
        let seeded = initial_scope(&[RequirementSeed {
            kind: RequirementKind::Named,
            text: "the database has no demo accounts".to_string(),
            target: None,
            source_quote: "no demo accounts".to_string(),
            check: None,
            verification: Some(RequirementVerification {
                fingerprint: "v1:check".to_string(),
                registered_by: "call-commit".to_string(),
            }),
        }])
        .expect("seed");
        let replacement = |verification| ScopeOperation::Replace {
            requirement_id: "R1".to_string(),
            kind: RequirementKind::Named,
            text: "the database has no demo accounts".to_string(),
            target: None,
            source_quote: "no demo accounts".to_string(),
            check: None,
            verification,
            reason: None,
        };
        assert_eq!(
            apply_scope_declaration(&seeded.items, &declaration(vec![replacement(None)])),
            Err(ScopeError::CannotDropVerification {
                id: "R1".to_string()
            })
        );
        let new_check = RequirementVerification {
            fingerprint: "v1:check-v2".to_string(),
            registered_by: "call-replace".to_string(),
        };
        let projected = apply_scope_declaration(
            &seeded.items,
            &declaration(vec![replacement(Some(new_check.clone()))]),
        )
        .expect("the replacement registers its own check");
        assert_eq!(projected.items[0].state, RequirementState::Superseded);
        assert_eq!(
            projected.items[0].verification, seeded.items[0].verification,
            "the retired requirement keeps the check it was judged by"
        );
        assert_eq!(projected.items[1].verification.as_ref(), Some(&new_check));
    }

    /// A registered check is not a ceiling: a legitimate steer may replace the
    /// model judgement with a stronger mechanical requirement. The new
    /// requirement's own kind is the stronger contract, so the old
    /// registration is not owed.
    #[test]
    fn a_registered_named_requirement_may_strengthen_to_tests() {
        let seeded = initial_scope(&[RequirementSeed {
            kind: RequirementKind::Named,
            text: "the database has no demo accounts".to_string(),
            target: None,
            source_quote: "no demo accounts".to_string(),
            check: None,
            verification: Some(RequirementVerification {
                fingerprint: "v1:check".to_string(),
                registered_by: "call-commit".to_string(),
            }),
        }])
        .expect("seed");
        let replacement = ScopeOperation::Replace {
            requirement_id: "R1".to_string(),
            kind: RequirementKind::Tests,
            text: "the database assertion suite passes".to_string(),
            target: Some("make check".to_string()),
            source_quote: "run the database checks".to_string(),
            check: None,
            verification: None,
            reason: Some("the user asked for an executable check".to_string()),
        };
        let projected = apply_scope_declaration(&seeded.items, &declaration(vec![replacement]))
            .expect("a stronger mechanical contract may replace a registered judgement");
        assert_eq!(projected.items[0].state, RequirementState::Superseded);
        assert_eq!(
            projected.items[0].verification,
            seeded.items[0].verification
        );
        assert_eq!(projected.items[1].kind, RequirementKind::Tests);
        assert_eq!(projected.items[1].target.as_deref(), Some("make check"));
        assert!(projected.items[1].verification.is_none());
    }

    #[test]
    fn amending_a_registered_named_requirement_into_tests_drops_the_registration() {
        let seeded = initial_scope(&[RequirementSeed {
            kind: RequirementKind::Named,
            text: "the database has no demo accounts".to_string(),
            target: None,
            source_quote: "no demo accounts".to_string(),
            check: None,
            verification: Some(RequirementVerification {
                fingerprint: "v1:check".to_string(),
                registered_by: "call-commit".to_string(),
            }),
        }])
        .expect("seed");
        let projected = apply_scope_declaration(
            &seeded.items,
            &declaration(vec![ScopeOperation::Amend {
                requirement_id: "R1".to_string(),
                kind: Some(RequirementKind::Tests),
                target: Some("make check".to_string()),
                text: Some("the database assertion suite passes".to_string()),
                source_quote: "run the database checks".to_string(),
                check: None,
                verification: None,
                reason: None,
            }]),
        )
        .expect("a stronger mechanical kind may replace the registered judgement");
        assert_eq!(projected.items[0].kind, RequirementKind::Tests);
        assert_eq!(projected.items[0].target.as_deref(), Some("make check"));
        assert!(
            projected.items[0].verification.is_none(),
            "the tests requirement owns its mechanical check; it must not inherit the named registration"
        );
        let history = [seeded.events, projected.events].concat();
        assert_eq!(fold_scope_events(&history).unwrap(), projected.items);
    }

    #[test]
    fn folding_the_history_reproduces_the_current_set() {
        let current = vec![item("R1", RequirementKind::Named, "docs")];
        let projected = apply_scope_declaration(
            &current,
            &declaration(vec![
                ScopeOperation::Waive {
                    requirement_id: "R1".to_string(),
                    source_quote: "drop docs".to_string(),
                    reason: Some("user no longer needs it".to_string()),
                },
                add("tests", "add tests"),
                ScopeOperation::Restore {
                    requirement_id: "R1".to_string(),
                    source_quote: "docs are needed again".to_string(),
                    reason: None,
                },
            ]),
        )
        .expect("declaration applies");
        let partial = fold_scope_events(&projected.events)
            .expect_err("a window without its original add event must be refused");
        assert!(matches!(partial, ScopeError::CorruptHistory(_)));

        let full: Vec<AppliedRequirementEvent> = initial_scope(&[RequirementSeed {
            kind: RequirementKind::Named,
            text: "docs".to_string(),
            target: None,
            source_quote: "docs".to_string(),
            check: None,
            verification: None,
        }])
        .expect("initial scope")
        .events
        .into_iter()
        .chain(projected.events)
        .collect();
        let folded = fold_scope_events(&full).expect("full history folds");
        assert_eq!(
            ids(&folded),
            vec![("R1".to_string(), "active"), ("R2".to_string(), "active"),]
        );
        assert_eq!(folded[1].text, "tests");
    }

    #[test]
    fn a_declaration_is_bounded_and_requires_a_quote() {
        let empty = apply_scope_declaration(&[], &declaration(vec![])).expect_err("empty");
        assert_eq!(empty, ScopeError::EmptyOperations);

        let no_quote = apply_scope_declaration(&[], &declaration(vec![add("x", "   ")]))
            .expect_err("empty quote");
        assert_eq!(no_quote, ScopeError::EmptySourceQuote);

        let too_many = apply_scope_declaration(
            &[],
            &declaration(
                (0..=SCOPE_OPERATIONS_MAX)
                    .map(|i| add(&format!("r{i}"), "q"))
                    .collect(),
            ),
        )
        .expect_err("too many");
        assert_eq!(too_many, ScopeError::TooManyOperations);
    }
    #[test]
    fn a_named_requirement_declaration_preserves_its_registered_check() {
        let declaration = serde_json::json!({
            "operations": [{
                "op": "add",
                "kind": "named",
                "text": "The rebuilt store has no demo accounts",
                "source_quote": "no demo accounts",
                "verification": {
                    "fingerprint": "v1:invocation",
                    "registered_by": "call-definition"
                }
            }]
        });
        let parsed: ScopeDeclaration = serde_json::from_value(declaration.clone()).unwrap();
        let applied = apply_scope_declaration(&[], &parsed).unwrap();
        assert_eq!(fold_scope_events(&applied.events).unwrap(), applied.items);
        assert!(applied.items[0].verification.is_some());
        assert_eq!(
            serde_json::to_value(&parsed).unwrap()["operations"][0]["verification"],
            declaration["operations"][0]["verification"]
        );
    }

    #[test]
    fn amending_a_registered_requirement_preserves_or_replaces_its_check() {
        let original = RequirementVerification {
            fingerprint: "original".into(),
            registered_by: "call-commit".into(),
        };
        let replacement = RequirementVerification {
            fingerprint: "replacement".into(),
            registered_by: "call-amend".into(),
        };
        let seed = initial_scope(&[RequirementSeed {
            kind: RequirementKind::Named,
            text: "The service runs the new build".into(),
            target: None,
            source_quote: "run the new build".into(),
            check: None,
            verification: Some(original.clone()),
        }])
        .unwrap();
        let amendment = |kind, text: Option<&str>, verification| ScopeOperation::Amend {
            requirement_id: "R1".into(),
            kind,
            text: text.map(str::to_string),
            target: None,
            source_quote: "run the new build".into(),
            check: None,
            verification,
            reason: None,
        };
        let unchanged =
            apply_scope_declaration(&seed.items, &declaration(vec![amendment(None, None, None)]))
                .unwrap();
        assert_eq!(unchanged.items[0].verification.as_ref(), Some(&original));
        let changed_outcome = Some("The service serves the new build");
        assert_eq!(
            apply_scope_declaration(
                &unchanged.items,
                &declaration(vec![amendment(None, changed_outcome, None)])
            ),
            Err(ScopeError::VerificationNeedsRegistration { id: "R1".into() })
        );
        let updated = apply_scope_declaration(
            &unchanged.items,
            &declaration(vec![amendment(
                None,
                changed_outcome,
                Some(replacement.clone()),
            )]),
        )
        .unwrap();
        let events = [seed.events, unchanged.events, updated.events].concat();
        let replayed = fold_scope_events(&events).unwrap();
        assert_eq!(replayed[0].verification.as_ref(), Some(&replacement));
        assert_eq!(replayed, updated.items);
        assert_eq!(
            apply_scope_declaration(
                &replayed,
                &declaration(vec![amendment(
                    Some(RequirementKind::Constraint),
                    None,
                    None
                )])
            ),
            Err(ScopeError::EvidencePolicyDowngrade {
                id: "R1".to_string(),
                from: "deliverable",
                to: "constraint",
            })
        );
    }
}
