//! Memory Dream: consolidate claimed observations into curated Markdown topics.
//!
//! The store owns the durability protocol — claim snapshot, lease fencing,
//! plan persistence, archive, index and manifest convergence. This module owns
//! only the model call and the decision to run it:
//!
//! * automatic: after a capture committed observations, gated by the durable
//!   eligibility rule (enough pending observations, or old enough);
//! * manual: an explicit `/dream`, which skips eligibility but still respects
//!   the rollout's write permission and the canonical lease.
//!
//! Only the workspace scope is consolidated; the global scope is fed by
//! user-authored `/remember` notes, matching the reference. A crash after the
//! plan is persisted resumes through `resume_planned`, so a model call is
//! never repeated for an operation that already reached the durable plan
//! boundary.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use leveler_core::SessionId;
use leveler_memory::config::MemoryV2Rollout;
use leveler_memory::{
    ConsolidationInput, ConsolidationLease, ConsolidationStatus, DreamClaimRequest,
    DreamEligibilityConfig, DreamTriggerDisposition, TopicOperation, V2ConsolidationError,
    V2ConsolidationStore, V2MemoryScope,
};
use leveler_model::{
    ContentPart, FinishReason, ModelRef, ModelRequest, ModelRuntime, Role, ToolChoice,
    ToolDefinition,
};
use leveler_storage::Database;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::{MemoryActivity, MemoryActivityKind, MemoryRuntime};
use crate::config_manager::ConfigManager;

/// A consolidation can span many observations and existing topics; the
/// reference allows 30 minutes.
const DREAM_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Long enough that a live model call is never reclaimed; a crashed worker's
/// claim still becomes reclaimable within one working day.
const DREAM_LEASE: Duration = Duration::from_secs(6 * 60 * 60);
const DREAM_MAX_OUTPUT_TOKENS: u32 = 16 * 1024;
/// The reference's automatic trigger thresholds.
const DREAM_MIN_PENDING_COUNT: usize = 20;
const DREAM_MAX_PENDING_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The forced tool the Dream model answers with. CodeLeveler's model protocol
/// has no `response_format`, so the reference's plan schema becomes this tool's
/// input schema, exactly as it does for capture.
const DREAM_TOOL_NAME: &str = "record_memory_plan";

const DREAM_SYSTEM_PROMPT: &str = "Consolidate only the supplied claimed observations \
            into the supplied curated topics. Topics are reference notes for a future agent that has \
            not seen any conversation and will read them before starting related work. A topic covers \
            one broad subject area (a system, a repository area, a tool, a person, or a workflow); its \
            sub-areas are `##` sections, not separate topics. A workspace usually needs fewer than ten \
            topics. Treat `topic_hint` as a section suggestion; add to the existing topic whose area \
            covers an observation rather than creating a new one, and split only above about 16 KB. \
            Each topic starts with a `# Title` and one sentence stating what it covers (an index shows \
            only those two lines), then durable facts as short standalone statements under `##` \
            headings: file paths, constants, commands, invariants, decisions, and user preferences. \
            Put changing state under `## Recent state` or omit it. Do not narrate conversations. Drop \
            facts that newer observations contradict or make stale. \
            Return one JSON object with an `operations` array. \
            Every operation must have an `op` field identifying its type: create, update, delete, \
            rename, merge, or split. Use exactly the fields listed for each type; do not add other fields. \
            create and update: op, path, content, evidence. delete: op, path, evidence. \
            rename: op, from, to, content, evidence. merge: op, sources, destination, content, evidence. \
            split: op, source, destinations, evidence. evidence and sources are arrays of path strings. \
            destinations is an array of objects containing exactly path and content. All other fields \
            are strings. Example: {\"operations\":[{\"op\":\"create\",\"path\":\"topics/preferences.md\",\
            \"content\":\"# Reply preferences\\nHow the user wants answers written.\\n\\n## Style\\n\
            - Prefer concise answers.\",\"evidence\":[\"observations/_inbox/example.md\"]}]}. \
            The example is illustrative; use only facts and exact evidence paths supplied in the input. \
            Every operation must cite one or more exact claimed observation paths in `evidence`. Every topic path \
            must be exactly `topics/<slug>.md`: one markdown file directly in `topics/`, no subdirectories. A slug is \
            lowercase letters, digits, and hyphens, for example `topics/preferences.md`. Preserve still-valid facts. \
            Do not include `<!-- memory-v2 provenance -->` comments in content; they are stripped and not stored. \
            Do not mention or attempt tools, shell, workspace files, MEMORY.md, databases, archive, \
            extraction, or another Dream.";

/// Which entry point asked for this Dream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DreamInvocation {
    Automatic,
    Manual,
}

/// What one Dream pass did. Every word here is a mechanical outcome, not a
/// model claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DreamOutcome {
    Disabled,
    WorkspaceEphemeral,
    Busy,
    Ineligible,
    NoWork,
    Recovered { observations: usize, topics: usize },
    Committed { observations: usize, topics: usize },
    Shadow { observations: usize },
    Cancelled,
    Failed(String),
}

impl DreamOutcome {
    /// The run reached a settled state the user asked for. A disabled pipeline,
    /// a lost worker slot, a failed call or a cancellation is reported as such.
    pub(crate) fn succeeded(&self) -> bool {
        matches!(
            self,
            Self::NoWork | Self::Recovered { .. } | Self::Committed { .. } | Self::Shadow { .. }
        )
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Disabled => "Dream is disabled by the current memory configuration.".to_owned(),
            Self::WorkspaceEphemeral => super::WORKSPACE_EPHEMERAL_MESSAGE.to_owned(),
            Self::Busy => "Another Dream is already running.".to_owned(),
            Self::Ineligible => "Not enough new observations yet; no Dream was started.".to_owned(),
            Self::NoWork => "There are no pending observations to consolidate.".to_owned(),
            Self::Recovered {
                observations,
                topics,
            } => format!(
                "Recovered an interrupted Dream: {observations} observations consolidated into {topics} topic changes."
            ),
            Self::Committed {
                observations,
                topics,
            } => format!(
                "Dream consolidated {observations} observations into {topics} topic changes."
            ),
            Self::Shadow { observations } => format!(
                "Shadow Dream validated a plan for {observations} observations; no topics were changed."
            ),
            Self::Cancelled => {
                "Dream was cancelled; its claim was released for a later retry.".to_owned()
            }
            Self::Failed(error) => format!("Dream failed: {error}"),
        }
    }
}

/// Start an automatic Dream after capture found observations.
///
/// Non-blocking: the caller is the session's capture worker and must not wait
/// for a 30-minute consolidation.
pub(crate) async fn after_capture(
    runtime: &MemoryRuntime,
    config: &Arc<ConfigManager>,
    db: &Database,
    session_id: &SessionId,
    model: &ModelRef,
) {
    if !runtime
        .config_for(Some(session_id.as_str()))
        .can_run_automatic_dream()
    {
        return;
    }
    let Some(cancel) = runtime.acquire_dream_worker().await else {
        // The running Dream observes the pending trigger on its next pass.
        return;
    };
    let runtime = runtime.clone();
    let config = config.clone();
    let db = db.clone();
    let session_id = session_id.clone();
    let model = model.clone();
    tokio::spawn(async move {
        let outcome = execute(
            &runtime,
            &config,
            &db,
            &session_id,
            &model,
            DreamInvocation::Automatic,
            &cancel,
        )
        .await;
        tracing::info!(outcome = %outcome.message(), "memory-v2 automatic Dream finished");
        runtime.release_dream_worker().await;
    });
}

/// Run one manual Dream pass and return its mechanical outcome.
pub(crate) async fn run_manual(
    runtime: &MemoryRuntime,
    config: &Arc<ConfigManager>,
    db: &Database,
    session_id: &SessionId,
    model: &ModelRef,
) -> DreamOutcome {
    if !runtime
        .config_for(Some(session_id.as_str()))
        .can_run_manual_dream()
    {
        return DreamOutcome::Disabled;
    }
    let Some(cancel) = runtime.acquire_dream_worker().await else {
        return DreamOutcome::Busy;
    };
    let outcome = execute(
        runtime,
        config,
        db,
        session_id,
        model,
        DreamInvocation::Manual,
        &cancel,
    )
    .await;
    runtime.release_dream_worker().await;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    runtime: &MemoryRuntime,
    config: &Arc<ConfigManager>,
    db: &Database,
    session_id: &SessionId,
    model: &ModelRef,
    invocation: DreamInvocation,
    cancel: &CancellationToken,
) -> DreamOutcome {
    let memory_config = runtime.config_for(Some(session_id.as_str()));
    let allowed = match invocation {
        DreamInvocation::Automatic => memory_config.can_run_automatic_dream(),
        DreamInvocation::Manual => memory_config.can_run_manual_dream(),
    };
    if !allowed {
        return DreamOutcome::Disabled;
    }
    // Dream consolidates the workspace scope only; a temporary working
    // directory has none.
    if runtime.is_workspace_ephemeral() {
        return DreamOutcome::WorkspaceEphemeral;
    }
    let shadow = memory_config.rollout == MemoryV2Rollout::Shadow;
    let scope = session_id.as_str().to_owned();
    let storage = runtime.storage();
    let global_dir = storage.global_dir().to_path_buf();
    let workspace_dir = storage.workspace_dir().to_path_buf();
    let scope_dir = workspace_dir.clone();
    let route = runtime.model_override().unwrap_or_else(|| model.clone());
    let registry = config.snapshot().registry.clone();

    let store = match tokio::task::spawn_blocking({
        let global_dir = global_dir.clone();
        let workspace_dir = workspace_dir.clone();
        move || {
            V2ConsolidationStore::open(
                scope_dir,
                V2MemoryScope::Workspace,
                &global_dir,
                &workspace_dir,
            )
            .map(Arc::new)
            .map_err(|error| error.to_string())
        }
    })
    .await
    {
        Ok(Ok(store)) => store,
        Ok(Err(error)) => return DreamOutcome::Failed(error),
        Err(error) => return DreamOutcome::Failed(format!("Dream store task failed: {error}")),
    };

    if invocation == DreamInvocation::Automatic {
        let eligibility = DreamEligibilityConfig {
            min_pending_count: DREAM_MIN_PENDING_COUNT,
            max_pending_age: DREAM_MAX_PENDING_AGE,
        };
        let evaluate_store = store.clone();
        let disposition = tokio::task::spawn_blocking(move || {
            let now = leveler_core::now().timestamp();
            let result = if shadow {
                evaluate_store.on_capture_completed_shadow(now, eligibility)
            } else {
                evaluate_store.on_capture_completed(now, eligibility)
            };
            result
                .map(|eligibility| eligibility.disposition)
                .map_err(|error| error.to_string())
        })
        .await;
        match disposition {
            Ok(Ok(DreamTriggerDisposition::Ready)) => {}
            Ok(Ok(DreamTriggerDisposition::Coalesced)) => return DreamOutcome::Busy,
            Ok(Ok(DreamTriggerDisposition::Ineligible)) => return DreamOutcome::Ineligible,
            Ok(Err(error)) => return DreamOutcome::Failed(error),
            Err(error) => {
                return DreamOutcome::Failed(format!("Dream eligibility task failed: {error}"));
            }
        }
    }

    let owner = format!("{}-{}", std::process::id(), leveler_core::new_uuid_string());
    let request = DreamClaimRequest {
        owner,
        now: leveler_core::now().timestamp(),
        duration: DREAM_LEASE,
    };
    let claim_store = store.clone();
    let claimed = tokio::task::spawn_blocking(move || {
        if shadow {
            claim_store.claim_shadow(&request)
        } else {
            claim_store.claim(&request)
        }
    })
    .await;
    let lease = match claimed {
        Ok(Ok(Some(lease))) => lease,
        Ok(Ok(None)) => return DreamOutcome::NoWork,
        Ok(Err(V2ConsolidationError::Busy)) => return DreamOutcome::Busy,
        Ok(Err(error)) => return DreamOutcome::Failed(error.to_string()),
        Err(error) => return DreamOutcome::Failed(format!("Dream claim task failed: {error}")),
    };

    let mut guard = DreamLeaseGuard::new(store.clone(), lease);
    if cancel.is_cancelled() {
        guard.release("Dream task cancelled").await;
        return DreamOutcome::Cancelled;
    }
    runtime.record_activity(
        &scope,
        MemoryActivity {
            kind: MemoryActivityKind::Dreaming,
            active: true,
            ..Default::default()
        },
    );

    let resume_store = store.clone();
    let resume_lease = guard.lease().clone();
    let resumed = tokio::task::spawn_blocking(move || {
        resume_store
            .resume_planned(&resume_lease, leveler_core::now().timestamp())
            .map_err(|error| error.to_string())
    })
    .await;
    match resumed {
        Ok(Ok(Some(result))) => {
            guard.disarm();
            finish_activity(runtime, &scope, true);
            return DreamOutcome::Recovered {
                observations: result.observation_count,
                topics: result.affected_topics.len(),
            };
        }
        Ok(Ok(None)) => {}
        Ok(Err(error)) => {
            guard.release(&error).await;
            finish_activity(runtime, &scope, false);
            return DreamOutcome::Failed(error);
        }
        Err(error) => {
            let detail = format!("Dream resume task failed: {error}");
            guard.release(&detail).await;
            finish_activity(runtime, &scope, false);
            return DreamOutcome::Failed(detail);
        }
    }

    let input_store = store.clone();
    let input_lease = guard.lease().clone();
    let input = tokio::task::spawn_blocking(move || {
        input_store
            .consolidation_input(&input_lease, leveler_core::now().timestamp())
            .map_err(|error| error.to_string())
    })
    .await;
    let input = match input {
        Ok(Ok(input)) => input,
        Ok(Err(error)) => {
            guard.release(&error).await;
            finish_activity(runtime, &scope, false);
            return DreamOutcome::Failed(error);
        }
        Err(error) => {
            let detail = format!("Dream input task failed: {error}");
            guard.release(&detail).await;
            finish_activity(runtime, &scope, false);
            return DreamOutcome::Failed(detail);
        }
    };

    let plan = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            guard.release("Dream task cancelled").await;
            finish_activity(runtime, &scope, false);
            return DreamOutcome::Cancelled;
        }
        plan = request_plan(&registry, &route, db, session_id, &input, cancel) => plan,
    };
    let operations = match plan {
        Ok(operations) => operations,
        Err(error) => {
            guard.release(&error).await;
            finish_activity(runtime, &scope, false);
            return DreamOutcome::Failed(error);
        }
    };
    if cancel.is_cancelled() {
        guard.release("Dream task cancelled").await;
        finish_activity(runtime, &scope, false);
        return DreamOutcome::Cancelled;
    }

    let observations = guard.lease().observations.len();
    let (store, lease) = guard.into_parts();
    // Keep a guard across the commit window so a panicking task or a dropped
    // future cannot strand the canonical lock until its six-hour expiry.
    let mut commit_guard = DreamLeaseGuard::new(store.clone(), lease.clone());
    let committed = tokio::task::spawn_blocking(move || {
        let now = leveler_core::now().timestamp();
        if shadow {
            store
                .complete_shadow(&lease, &operations, now)
                .map_err(|error| error.to_string())
        } else {
            let result = store.commit(&lease, &operations, now);
            if let Err(error) = &result {
                let _ = store.fail_retryable(&lease, now, &format!("Dream commit failed: {error}"));
            }
            result.map_err(|error| error.to_string())
        }
    })
    .await;
    match committed {
        Ok(Ok(result)) => {
            commit_guard.disarm();
            finish_activity(runtime, &scope, true);
            if shadow || result.status == ConsolidationStatus::Shadow {
                DreamOutcome::Shadow { observations }
            } else {
                DreamOutcome::Committed {
                    observations,
                    topics: result.affected_topics.len(),
                }
            }
        }
        Ok(Err(error)) => {
            // `complete_shadow` released its own claim on a rejected plan;
            // `commit` was released above. Nothing else owns this lease.
            commit_guard.disarm();
            finish_activity(runtime, &scope, false);
            DreamOutcome::Failed(error)
        }
        Err(error) => {
            let detail = format!("Dream commit task failed: {error}");
            commit_guard.release(&detail).await;
            finish_activity(runtime, &scope, false);
            DreamOutcome::Failed(detail)
        }
    }
}

fn finish_activity(runtime: &MemoryRuntime, session_scope: &str, success: bool) {
    runtime.record_activity(
        session_scope,
        MemoryActivity {
            kind: if success {
                MemoryActivityKind::Dreaming
            } else {
                MemoryActivityKind::Failed
            },
            active: false,
            last_dream_at: success.then(|| leveler_core::now().timestamp()),
            ..Default::default()
        },
    );
}

async fn request_plan(
    registry: &Arc<leveler_provider::ProviderRegistry>,
    route: &ModelRef,
    db: &Database,
    session_id: &SessionId,
    input: &ConsolidationInput,
    cancel: &CancellationToken,
) -> Result<Vec<TopicOperation>, String> {
    let payload = serde_json::json!({
        "operation_id": input.operation_id,
        "claimed_observations": input.observations,
        "existing_topics": input.topics,
    })
    .to_string();
    let mut request = ModelRequest::new(
        route.clone(),
        vec![
            leveler_model::Message::text(Role::System, DREAM_SYSTEM_PROMPT),
            leveler_model::Message::text(Role::User, payload),
        ],
    );
    request.tools = vec![ToolDefinition {
        name: DREAM_TOOL_NAME.to_owned(),
        description: "Record the topic operations that consolidate the claimed observations."
            .to_owned(),
        input_schema: dream_plan_schema(),
    }];
    request.tool_choice = ToolChoice::Tool(DREAM_TOOL_NAME.to_owned());
    request.max_output_tokens = Some(DREAM_MAX_OUTPUT_TOKENS);
    request.temperature = Some(0.0);
    // A forced tool choice is incompatible with thinking on every route (the
    // Anthropic-compatible encoder rejects it outright), and consolidation is
    // mechanical. Capture disables thinking for the same reason.
    request.thinking_disabled = true;

    let started = std::time::Instant::now();
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err("Dream task cancelled".to_owned()),
        result = tokio::time::timeout(DREAM_TIMEOUT, registry.generate(request, cancel.clone())) => result,
    };
    let outcome = match result {
        Ok(result) => result,
        Err(_) => Err(leveler_model::ModelError::new(
            leveler_model::ModelErrorKind::Timeout,
            format!(
                "memory Dream timed out after {} seconds",
                DREAM_TIMEOUT.as_secs()
            ),
        )),
    };
    let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    crate::observability::record_auxiliary_call_in_registry(
        registry,
        db,
        session_id,
        leveler_storage::ModelCallKind::MemoryDream,
        &route.provider,
        &route.model,
        None,
        &outcome,
        latency_ms,
        None,
        None,
    )
    .await
    .map_err(|error| format!("could not record the Dream call: {error}"))?;
    let response = outcome.map_err(|error| format!("Dream model failed: {error}"))?;
    if response.finish_reason == FinishReason::Length {
        return Err("Dream output was truncated".to_owned());
    }
    let arguments = response
        .message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::ToolCall { call } if call.name == DREAM_TOOL_NAME => {
                Some(call.arguments.clone())
            }
            _ => None,
        })
        .ok_or_else(|| "Dream model did not call the required tool".to_owned())?;
    parse_dream_plan(&arguments)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2DreamPlan {
    operations: Vec<V2DreamPlanOperation>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum V2DreamPlanOperation {
    Create {
        path: String,
        content: String,
        evidence: Vec<String>,
    },
    Update {
        path: String,
        content: String,
        evidence: Vec<String>,
    },
    Delete {
        path: String,
        evidence: Vec<String>,
    },
    Rename {
        from: String,
        to: String,
        content: String,
        evidence: Vec<String>,
    },
    Merge {
        sources: Vec<String>,
        destination: String,
        content: String,
        evidence: Vec<String>,
    },
    Split {
        source: String,
        destinations: Vec<V2DreamDestination>,
        evidence: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2DreamDestination {
    path: String,
    content: String,
}

/// Parse the forced tool call's arguments into typed operations.
pub(crate) fn parse_dream_plan(value: &serde_json::Value) -> Result<Vec<TopicOperation>, String> {
    let plan: V2DreamPlan = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid Dream plan: {error}"))?;
    if plan.operations.is_empty() {
        return Err("invalid Dream plan: operations must not be empty".to_owned());
    }
    let paths = |values: Vec<String>| values.into_iter().map(PathBuf::from).collect();
    Ok(plan
        .operations
        .into_iter()
        .map(|operation| match operation {
            V2DreamPlanOperation::Create {
                path,
                content,
                evidence,
            } => TopicOperation::Create {
                path: path.into(),
                content,
                evidence: paths(evidence),
            },
            V2DreamPlanOperation::Update {
                path,
                content,
                evidence,
            } => TopicOperation::Update {
                path: path.into(),
                content,
                evidence: paths(evidence),
            },
            V2DreamPlanOperation::Delete { path, evidence } => TopicOperation::Delete {
                path: path.into(),
                evidence: paths(evidence),
            },
            V2DreamPlanOperation::Rename {
                from,
                to,
                content,
                evidence,
            } => TopicOperation::Rename {
                from: from.into(),
                to: to.into(),
                content,
                evidence: paths(evidence),
            },
            V2DreamPlanOperation::Merge {
                sources,
                destination,
                content,
                evidence,
            } => TopicOperation::Merge {
                sources: paths(sources),
                destination: destination.into(),
                content,
                evidence: paths(evidence),
            },
            V2DreamPlanOperation::Split {
                source,
                destinations,
                evidence,
            } => TopicOperation::Split {
                source: source.into(),
                destinations: destinations
                    .into_iter()
                    .map(|destination| (destination.path.into(), destination.content))
                    .collect(),
                evidence: paths(evidence),
            },
        })
        .collect())
}

/// The bare JSON schema for the forced tool, mirroring [`V2DreamPlan`] and the
/// reference's `dream_plan_schema`.
pub(crate) fn dream_plan_schema() -> serde_json::Value {
    let strings = serde_json::json!({ "type": "array", "items": { "type": "string" } });
    let string = serde_json::json!({ "type": "string" });
    let operation = |op: &str, fields: &[(&str, serde_json::Value)]| {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "op".to_owned(),
            serde_json::json!({ "type": "string", "enum": [op] }),
        );
        let mut required = vec![serde_json::Value::from("op")];
        for (name, schema) in fields {
            properties.insert((*name).to_owned(), schema.clone());
            required.push(serde_json::Value::from(*name));
        }
        properties.insert("evidence".to_owned(), strings.clone());
        required.push(serde_json::Value::from("evidence"));
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": required,
            "properties": properties
        })
    };
    let destination = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["path", "content"],
        "properties": { "path": string, "content": string }
    });
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["operations"],
        "properties": {
            "operations": {
                "type": "array",
                "items": {
                    "anyOf": [
                        operation("create", &[("path", string.clone()), ("content", string.clone())]),
                        operation("update", &[("path", string.clone()), ("content", string.clone())]),
                        operation("delete", &[("path", string.clone())]),
                        operation("rename", &[("from", string.clone()), ("to", string.clone()), ("content", string.clone())]),
                        operation("merge", &[("sources", strings.clone()), ("destination", string.clone()), ("content", string.clone())]),
                        operation("split", &[("source", string.clone()), ("destinations", serde_json::json!({ "type": "array", "items": destination }))]),
                    ]
                }
            }
        }
    })
}

/// Owns a live consolidation lease. Any exit that does not explicitly
/// `disarm` releases the claim so a later pass does not wait for the lease to
/// expire.
struct DreamLeaseGuard {
    resources: Option<(Arc<V2ConsolidationStore>, ConsolidationLease)>,
}

impl DreamLeaseGuard {
    fn new(store: Arc<V2ConsolidationStore>, lease: ConsolidationLease) -> Self {
        Self {
            resources: Some((store, lease)),
        }
    }

    fn lease(&self) -> &ConsolidationLease {
        &self
            .resources
            .as_ref()
            .expect("an armed Dream lease guard owns its lease")
            .1
    }

    fn disarm(&mut self) {
        self.resources = None;
    }

    fn into_parts(mut self) -> (Arc<V2ConsolidationStore>, ConsolidationLease) {
        self.resources
            .take()
            .expect("an armed Dream lease guard owns its resources")
    }

    async fn release(mut self, error: &str) {
        let Some((store, lease)) = self.resources.take() else {
            return;
        };
        let error = error.to_owned();
        let now = leveler_core::now().timestamp();
        let released =
            tokio::task::spawn_blocking(move || store.fail_retryable(&lease, now, &error)).await;
        match released {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "memory-v2 Dream lease release failed"),
            Err(error) => tracing::warn!(%error, "memory-v2 Dream lease release task failed"),
        }
    }
}

impl Drop for DreamLeaseGuard {
    fn drop(&mut self) {
        let Some((store, lease)) = self.resources.take() else {
            return;
        };
        let now = leveler_core::now().timestamp();
        let release = move || {
            if let Err(error) =
                store.fail_retryable(&lease, now, "Dream worker exited before completion")
            {
                tracing::warn!(%error, "memory-v2 abandoned Dream lease release failed");
            }
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(release);
        } else {
            release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_parses_every_operation_shape() {
        let plan = serde_json::json!({
            "operations": [
                {"op": "create", "path": "topics/a.md", "content": "# A", "evidence": ["observations/_inbox/a.md"]},
                {"op": "update", "path": "topics/a.md", "content": "# A", "evidence": ["observations/_inbox/b.md"]},
                {"op": "delete", "path": "topics/old.md", "evidence": ["observations/_inbox/c.md"]},
                {"op": "rename", "from": "topics/a.md", "to": "topics/b.md", "content": "# B", "evidence": ["observations/_inbox/d.md"]},
                {"op": "merge", "sources": ["topics/a.md", "topics/b.md"], "destination": "topics/c.md", "content": "# C", "evidence": ["observations/_inbox/e.md"]},
                {"op": "split", "source": "topics/c.md", "destinations": [{"path": "topics/d.md", "content": "# D"}], "evidence": ["observations/_inbox/f.md"]}
            ]
        });
        assert_eq!(parse_dream_plan(&plan).expect("plan").len(), 6);
    }

    #[test]
    fn an_empty_plan_is_rejected_instead_of_reaching_the_durable_boundary() {
        assert!(parse_dream_plan(&serde_json::json!({"operations": []})).is_err());
    }
}
