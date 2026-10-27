//! Shared fixture for the Stage 3 memory context/injection tests.
//!
//! These tests drive the production executor with a Markdown-backed
//! `MemoryService`, so what they assert is the request the provider would see,
//! not an internal helper.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use leveler_agent::{AgentEvent, Executor, NoopSink};
use leveler_core::{RequestId, ToolCallId};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_memory::v2::{MemoryScope, ScopeRoots};
use leveler_memory::{MemoryRoots, MemoryService, ServiceScope};
use leveler_model::{
    ContentPart, FinishReason, Message, ModelError, ModelErrorKind, ModelEvent, ModelEventStream,
    ModelProfile, ModelRef, ModelRequest, ModelResponse, ModelRuntime, Role, TokenUsage, ToolCall,
};
use leveler_tools::ToolContext;
use tempfile::TempDir;

/// A project + global (+ local under a git repo) Markdown store.
pub struct Fixture {
    dir: TempDir,
    pub project_current: PathBuf,
    pub project_state: PathBuf,
    pub global_current: PathBuf,
    pub global_state: PathBuf,
    pub local_current: PathBuf,
    pub local_state: PathBuf,
    pub repo: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // The agent workspace is separate from the memory stores: a memory
        // write must not change the repo listing that sits in the prefix.
        std::fs::create_dir_all(dir.path().join("workspace")).unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .arg("-q")
            .status()
            .expect("git is required for local scope");
        assert!(status.success(), "git init failed");
        Self {
            project_current: dir.path().join("project/current"),
            project_state: dir.path().join("project/state"),
            global_current: dir.path().join("global/current"),
            global_state: dir.path().join("global/state"),
            local_current: repo.join(".leveler/local/current"),
            local_state: dir.path().join("local/state"),
            repo,
            workspace: dir.path().join("workspace"),
            dir,
        }
    }

    pub fn project_roots(&self) -> ScopeRoots {
        ScopeRoots::new(
            MemoryScope::Project,
            &self.project_current,
            &self.project_state,
        )
    }

    pub fn global_roots(&self) -> ScopeRoots {
        ScopeRoots::new(
            MemoryScope::Global,
            &self.global_current,
            &self.global_state,
        )
    }

    pub fn local_roots(&self) -> ScopeRoots {
        ScopeRoots::new(MemoryScope::Local, &self.local_current, &self.local_state)
            .with_repo_root(&self.repo)
    }

    pub fn roots(&self) -> MemoryRoots {
        MemoryRoots::new()
            .with_project(self.project_roots())
            .with_global(self.global_roots())
            .with_local(self.local_roots())
    }

    pub fn service(&self) -> Arc<MemoryService> {
        Arc::new(MemoryService::new(self.roots()))
    }

    pub fn workdir(&self) -> &Path {
        &self.workspace
    }
}

/// A process-unique token; the test binary has no uuid dependency.
pub fn unique(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn user() -> leveler_memory::v2::Actor {
    leveler_memory::v2::Actor::User {
        invocation: leveler_memory::v2::UserInvocationId::mint(unique("inv")),
    }
}

pub fn op() -> String {
    unique("op")
}

/// Write one explicit user fact through the facade and return its id.
pub fn fact(
    service: &MemoryService,
    scope: ServiceScope,
    subject: &str,
    title: &str,
    body: &str,
) -> String {
    let mut request = leveler_memory::RememberRequest::new(op(), scope, title, body, "preference");
    request.subject = Some(subject.to_string());
    service
        .remember(&user(), request)
        .unwrap()
        .applied_id()
        .expect("fact write must apply")
        .to_string()
}

fn candidate(
    title: &str,
    body: &str,
    key: Option<&str>,
    source: leveler_memory::CandidateSource,
) -> leveler_memory::MemoryCandidate {
    leveler_memory::MemoryCandidate::new(
        title,
        body,
        leveler_memory::CandidateKind::Free,
        key.map(str::to_string),
        source,
        Vec::new(),
    )
    .unwrap()
}

/// A model-inferred, non-standing fact: retrieval must earn its place.
pub fn model_fact(
    service: &MemoryService,
    scope: ServiceScope,
    subject: &str,
    title: &str,
    body: &str,
) -> String {
    let mut candidate = candidate(
        title,
        body,
        Some(subject),
        leveler_memory::CandidateSource::AgentProposed,
    );
    candidate.semantic_key = Some(subject.into());
    let actor = leveler_memory::v2::Actor::Model {
        session: leveler_core::SessionId::new("fixture-session"),
        turn: leveler_core::TurnId::new(unique("fixture-turn")),
        tool_call: leveler_core::ToolCallId::new(unique("fixture-call")),
    };
    service
        .admit_candidate(&actor, scope, &candidate, &unique("fixture-seed"))
        .unwrap()
        .applied_id()
        .expect("model fact must apply")
        .to_string()
}

/// Write one runtime-verified experience through the facade, so the derived
/// index sees the write exactly as it would in production.
pub fn experience(
    service: &MemoryService,
    scope: ServiceScope,
    category: &str,
    applies_when: &str,
    body: &str,
) -> String {
    let mut candidate = candidate(
        category,
        body,
        None,
        leveler_memory::CandidateSource::RuntimeObserved,
    );
    candidate.experience = Some(leveler_memory::ExperienceRecord {
        category: leveler_memory::ExperienceCategory::RootCause,
        verification: leveler_memory::ExperienceVerification::Verified,
        applies_when: applies_when.into(),
        invalid_if: None,
        evidence: vec![
            leveler_memory::ExperienceEvidence {
                kind: "failure".into(),
                reference: "failure-1".into(),
            },
            leveler_memory::ExperienceEvidence {
                kind: "resolution".into(),
                reference: "resolution-1".into(),
            },
        ],
    });
    let actor = leveler_memory::v2::Actor::Runtime {
        task: leveler_core::TaskId::new("fixture-task"),
    };
    service
        .admit_candidate(&actor, scope, &candidate, &unique("fixture-exp"))
        .unwrap()
        .applied_id()
        .expect("experience must apply")
        .to_string()
}

/// Scripted provider: replays responses and records every request.
pub struct ScriptedRuntime {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedRuntime {
    pub fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, response: ModelResponse) {
        self.responses.lock().unwrap().push_back(response);
    }

    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelRuntime for ScriptedRuntime {
    async fn generate(
        &self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        Err(ModelError::new(
            ModelErrorKind::Other,
            "this fixture only streams",
        ))
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| ModelError::new(ModelErrorKind::Other, "no scripted response"))?;
        let mut events = vec![Ok(ModelEvent::MessageStarted {
            request_id: response.request_id.clone(),
        })];
        for part in response.message.content {
            match part {
                ContentPart::Text { text } => {
                    events.push(Ok(ModelEvent::TextDelta { delta: text }));
                }
                ContentPart::ToolCall { call } => {
                    events.push(Ok(ModelEvent::ToolCallCompleted { call }));
                }
                _ => {}
            }
        }
        if response.usage.total() > 0 {
            events.push(Ok(ModelEvent::UsageUpdated {
                usage: response.usage,
            }));
        }
        events.push(Ok(ModelEvent::MessageCompleted {
            finish_reason: response.finish_reason,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }

    async fn profile(&self, _model: &ModelRef) -> Result<ModelProfile, ModelError> {
        unimplemented!()
    }
}

pub fn assistant_text(text: &str) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message::text(Role::Assistant, text),
        finish_reason: FinishReason::Stop,
        usage: TokenUsage::default(),
    }
}

pub fn assistant_call(id: &str, name: &str, arguments: serde_json::Value) -> ModelResponse {
    ModelResponse {
        request_id: RequestId::generate(),
        message: Message {
            origin: None,
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                call: ToolCall {
                    id: ToolCallId::new(id),
                    name: name.to_string(),
                    arguments,
                },
            }],
            route: None,
        },
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}

pub fn executor(
    workdir: &Path,
    service: Arc<MemoryService>,
    runtime: Arc<ScriptedRuntime>,
) -> Executor {
    let workspace = Workspace::new(workdir).unwrap();
    let tool_context = ToolContext::with_environment(
        workspace,
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            workdir.to_path_buf(),
            std::env::temp_dir(),
        )),
    );
    Executor::new(
        runtime,
        Arc::new(leveler_tools::default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .with_memory_expose(true)
    .with_memory_service(Some(service))
    .with_approver(Arc::new(AutoApprove))
}

pub async fn run_turn(executor: &Executor, request: &str) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    executor
        .run(
            request,
            &mut |event| events.push(event),
            &mut NoopSink,
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");
    events
}

pub fn recalled(events: &[AgentEvent]) -> Vec<&AgentEvent> {
    events
        .iter()
        .filter(|event| matches!(event, AgentEvent::MemoryRecalled { .. }))
        .collect()
}

/// The single `MemoryRecalled` receipt for a turn.
pub fn receipt(events: &[AgentEvent]) -> &AgentEvent {
    let hits = recalled(events);
    assert_eq!(hits.len(), 1, "expected exactly one MemoryRecalled");
    hits[0]
}

/// Write one runtime-verified experience under an explicit structured subject,
/// so it shares a canonical identity with a user fact about the same topic.
pub fn experience_about(
    service: &MemoryService,
    scope: ServiceScope,
    subject_key: &str,
    category: &str,
    applies_when: &str,
    body: &str,
) -> String {
    let mut candidate = candidate(
        category,
        body,
        None,
        leveler_memory::CandidateSource::RuntimeObserved,
    );
    candidate.semantic_key = Some(subject_key.to_string());
    candidate.experience = Some(leveler_memory::ExperienceRecord {
        category: leveler_memory::ExperienceCategory::RootCause,
        verification: leveler_memory::ExperienceVerification::Verified,
        applies_when: applies_when.into(),
        invalid_if: None,
        evidence: vec![
            leveler_memory::ExperienceEvidence {
                kind: "failure".into(),
                reference: "failure-1".into(),
            },
            leveler_memory::ExperienceEvidence {
                kind: "resolution".into(),
                reference: "resolution-1".into(),
            },
        ],
    });
    let actor = leveler_memory::v2::Actor::Runtime {
        task: leveler_core::TaskId::new("fixture-task"),
    };
    service
        .admit_candidate(&actor, scope, &candidate, &unique("fixture-exp"))
        .unwrap()
        .applied_id()
        .expect("experience must apply")
        .to_string()
}

/// The canonical identity of a subject label, as the service computes it.
pub fn subject_key(label: &str) -> String {
    leveler_memory::semantic_key_of(&leveler_memory::normalize_subject(label))
}
