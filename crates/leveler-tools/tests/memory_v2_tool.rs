//! Phase 4 gate: the memory tools are thin adapters over the Markdown service.
//!
//! The host stamps every call's session/turn/tool-call identity. No argument a
//! model passes can turn a tool call into a user decision, and the tools expose
//! no accept/reject action at all.

use std::sync::Arc;

use leveler_execution::{PermissionProfile, Workspace};
use leveler_memory::v2::{Actor, MemoryScope, ScopeRoots, UserInvocationId};
use leveler_memory::{MemoryLanes, MemoryRoots, MemoryService, RememberRequest, ServiceScope};
use leveler_tools::tools::{ForgetTool, MemoryRoot, MemoryTool, RememberTool};
use leveler_tools::{Tool, ToolContext};
use tokio_util::sync::CancellationToken;

struct Fixture {
    _dir: tempfile::TempDir,
    service: Arc<MemoryService>,
    ctx: ToolContext,
}

fn fixture() -> Fixture {
    let dir = tempfile::Builder::new()
        .prefix("leveler-memory-tools-")
        .tempdir()
        .unwrap();
    let project = ScopeRoots::new(
        MemoryScope::Project,
        dir.path().join("project/current"),
        dir.path().join("project/state"),
    );
    let global = ScopeRoots::new(
        MemoryScope::Global,
        dir.path().join("global/current"),
        dir.path().join("global/state"),
    );
    let service = Arc::new(MemoryService::new(
        MemoryRoots::markdown()
            .with_project(project)
            .with_global(global),
    ));
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut ctx = ToolContext::new(workspace, PermissionProfile::FullAccess);
    ctx.session_scope = Some(Arc::from("session-1"));
    ctx.turn_scope = Some(Arc::from("turn-1"));
    ctx.tool_call_scope = Some(Arc::from("call-1"));
    Fixture {
        _dir: dir,
        service,
        ctx,
    }
}

impl Fixture {
    fn root(&self) -> MemoryRoot {
        MemoryRoot::from_service(Some(self.service.clone()))
    }

    async fn remember(
        &self,
        args: serde_json::Value,
    ) -> Result<leveler_tools::ToolOutput, leveler_tools::ToolError> {
        RememberTool::new(self.root())
            .execute(args, self.ctx.clone(), CancellationToken::new())
            .await
    }

    async fn memory(
        &self,
        args: serde_json::Value,
    ) -> Result<leveler_tools::ToolOutput, leveler_tools::ToolError> {
        MemoryTool::new(self.root())
            .execute(args, self.ctx.clone(), CancellationToken::new())
            .await
    }

    fn active(&self, scope: ServiceScope) -> Vec<leveler_memory::MemoryView> {
        self.service
            .list(&Actor::External, scope, MemoryLanes::ALL, None)
            .unwrap()
    }
}

fn user() -> Actor {
    Actor::User {
        invocation: UserInvocationId::mint("test-user-command"),
    }
}

#[tokio::test]
async fn a_model_project_fact_is_active_model_inferred_and_readable() {
    let fixture = fixture();
    let output = fixture
        .remember(serde_json::json!({
            "title": "Default model is Flash",
            "body": "The project default model is Flash.",
            "subject": "project.default_model",
            "value": "Flash",
            "kind": "user_fact",
            "scope": "project"
        }))
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.content);
    assert!(
        output.content.starts_with("Remembered ["),
        "{}",
        output.content
    );

    let active = fixture.active(ServiceScope::Project);
    assert_eq!(active.len(), 1);
    let view = &active[0];
    assert_eq!(view.authority, "model_inferred");
    assert_eq!(view.status, "active");
    assert_eq!(view.value.as_deref(), Some("Flash"));

    // `read` must return the identity the model needs to update it later:
    // id, status, scope, and the revision the write path will require.
    let read = fixture
        .memory(serde_json::json!({
            "action": "read",
            "id": view.id,
            "scope": "project"
        }))
        .await
        .unwrap();
    assert!(!read.is_error, "{}", read.content);
    assert!(read.content.contains(&view.id), "{}", read.content);
    assert!(read.content.contains("status: active"), "{}", read.content);
    assert!(read.content.contains("scope: project"), "{}", read.content);
    assert!(read.content.contains("revision 1"), "{}", read.content);
}

#[tokio::test]
async fn a_model_global_fact_is_queued_and_never_becomes_active() {
    let fixture = fixture();
    let output = fixture
        .remember(serde_json::json!({
            "title": "User prefers Chinese",
            "body": "The user prefers Chinese replies.",
            "subject": "user.reply_language",
            "value": "zh-CN",
            "kind": "user_fact",
            "scope": "global"
        }))
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.content);
    assert!(
        output.content.contains("Queued for approval"),
        "{}",
        output.content
    );
    assert!(
        fixture.active(ServiceScope::Global).is_empty(),
        "a model write outside the project scope must not become current"
    );
    let pending = fixture
        .service
        .pending(&user(), ServiceScope::Global)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, "pending");
    assert_eq!(pending[0].authority, "model_inferred");

    // The tool surface must not offer a way for the model to settle it.
    let accept = fixture
        .memory(serde_json::json!({"action": "accept", "id": pending[0].id}))
        .await
        .unwrap();
    assert!(accept.is_error, "{}", accept.content);
    assert!(
        accept.content.contains("unknown action"),
        "{}",
        accept.content
    );
}

#[tokio::test]
async fn markdown_search_returns_lexical_hits_and_never_a_fake_empty_result() {
    let fixture = fixture();
    fixture
        .remember(serde_json::json!({
            "title": "Default model is Flash",
            "body": "The project default model is Flash.",
            "subject": "project.default_model",
            "value": "Flash",
            "kind": "decision",
            "scope": "project"
        }))
        .await
        .unwrap();

    // On the Markdown backend both actions run over the same derived lexical
    // index. The important contract is that a hit is a hit and a miss is a
    // real miss, never a claim of unavailability or an invented empty result.
    for action in ["search", "vector_search"] {
        let output = fixture
            .memory(serde_json::json!({"action": action, "query": "Flash"}))
            .await
            .unwrap();
        assert!(!output.is_error, "{action}: {}", output.content);
        assert!(
            output.content.contains("Default model is Flash"),
            "{action}: {}",
            output.content
        );
        assert!(
            !output.content.contains("No matching memories"),
            "{action}: {}",
            output.content
        );
    }
}

#[tokio::test]
async fn update_requires_the_current_revision_and_advances_it() {
    let fixture = fixture();
    fixture
        .remember(serde_json::json!({
            "title": "Default model is Flash",
            "body": "The project default model is Flash.",
            "subject": "project.default_model",
            "value": "Flash",
            "kind": "decision",
            "scope": "project"
        }))
        .await
        .unwrap();
    let id = fixture.active(ServiceScope::Project)[0].id.clone();

    // A trusted user command updates it directly and advances the revision;
    // this is the revision a later caller must read before writing.
    let applied = fixture
        .service
        .update(
            &user(),
            ServiceScope::Project,
            &id,
            1,
            leveler_memory::UpdateRequest {
                value: Some("Pro".to_string()),
                body: Some("The project default model is Pro.".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        matches!(applied, leveler_memory::WriteOutcome::Applied { .. }),
        "{applied:?}"
    );
    let current = fixture
        .service
        .read(&Actor::External, ServiceScope::Project, &id)
        .unwrap();
    assert_eq!(current.value.as_deref(), Some("Pro"));
    assert_eq!(current.revision, Some(2));

    // A model update carrying the revision it read is a lifecycle proposal;
    // it must not silently overwrite a user-owned record.
    let queued = fixture
        .memory(serde_json::json!({
            "action": "update",
            "id": id,
            "expected_revision": 2,
            "value": "Flash"
        }))
        .await
        .unwrap();
    assert!(!queued.is_error, "{}", queued.content);
    assert!(
        queued.content.contains("Queued for approval"),
        "{}",
        queued.content
    );

    // The same revision is now stale; a CAS conflict must be reported, not
    // silently applied over the newer value.
    let stale = fixture
        .memory(serde_json::json!({
            "action": "update",
            "id": id,
            "expected_revision": 1,
            "value": "Flash"
        }))
        .await;
    assert!(stale.is_err(), "a stale revision must not apply: {stale:?}");
}

#[tokio::test]
async fn a_model_forget_of_a_user_record_is_queued_not_applied() {
    let fixture = fixture();
    let mut request = RememberRequest::new(
        "user-command-1",
        ServiceScope::Project,
        "Default model is Pro",
        "The project default model is Pro.",
        "decision",
    );
    // A v2 user fact carries a structured identity; without one the store
    // rejects it rather than inventing a subject from the text.
    request.subject = Some("project.default_model".to_string());
    request.value = Some("Pro".to_string());
    let outcome = fixture.service.remember(&user(), request).unwrap();
    assert!(
        matches!(outcome, leveler_memory::WriteOutcome::Applied { .. }),
        "fixture user write was not applied: {outcome:?}"
    );
    let id = fixture.active(ServiceScope::Project)[0].id.clone();
    let before = fixture
        .service
        .read(&Actor::External, ServiceScope::Project, &id)
        .unwrap();
    assert_eq!(before.authority, "user_explicit");

    let output = ForgetTool::new(fixture.root())
        .execute(
            serde_json::json!({"id": id, "scope": "project"}),
            fixture.ctx.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.content);
    assert!(
        output.content.contains("Queued for approval"),
        "a user-owned record must not be removed by a model: {}",
        output.content
    );
    let after = fixture
        .service
        .read(&Actor::External, ServiceScope::Project, &id)
        .unwrap();
    assert_eq!(after.status, "active");
    assert_eq!(after.authority, "user_explicit");
}
