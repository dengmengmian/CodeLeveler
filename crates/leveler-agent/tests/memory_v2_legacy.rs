//! Stage 3: the legacy backend keeps its behavior.
//!
//! Stage 3 changes where dynamic recall sits (a request-ephemeral tail) and
//! how Markdown injection is accounted. The legacy JSON store still takes its
//! own path through the same executor; this test pins that path so the new
//! backend cannot regress the default one.

mod memory_v2_harness;

use std::sync::Arc;

use leveler_agent::{AgentEvent, Executor};
use leveler_execution::{AutoApprove, PermissionProfile, Workspace};
use leveler_memory::{MemoryStore, new_entry};
use leveler_model::ModelRef;
use leveler_tools::ToolContext;
use memory_v2_harness::{ScriptedRuntime, assistant_text, run_turn};

#[tokio::test]
async fn legacy_recall_still_reaches_the_request_and_now_rides_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let workspace_dir = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let mem = dir.path().join("memory");
    let store = MemoryStore::open(&mem).unwrap();
    let mut entry = new_entry("发布通道", "发布使用蓝色通道部署。", Vec::new());
    entry.kind = Some("preference".into());
    store.remember_deduplicated(entry).unwrap();
    let id = store.effective_active().unwrap()[0].id.clone();

    let runtime = Arc::new(ScriptedRuntime::new(vec![
        assistant_text("蓝色"),
        assistant_text("蓝色"),
    ]));
    let tool_context = ToolContext::with_environment(
        Workspace::new(&workspace_dir).unwrap(),
        PermissionProfile::Assisted,
        Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            workspace_dir.clone(),
            std::env::temp_dir(),
        )),
    );
    let executor = Executor::new(
        runtime.clone(),
        Arc::new(leveler_tools::default_registry()),
        tool_context,
        ModelRef::new("mock", "m"),
        8,
    )
    .with_memory_expose(true)
    .with_memory_root(Some(mem))
    .with_approver(Arc::new(AutoApprove));

    let first = run_turn(&executor, "发布通道是什么").await;
    run_turn(&executor, "发布通道是什么").await;
    let requests = runtime.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        // Recall is current-turn data, never part of the stable prefix.
        assert!(
            !request
                .control_context
                .prefix_text()
                .contains("蓝色通道部署"),
            "legacy recall leaked into the prefix"
        );
        assert!(
            request
                .control_context
                .trailing_text()
                .contains("发布使用蓝色通道部署。"),
            "legacy recall did not reach the request"
        );
    }
    assert_eq!(
        requests[0].control_context.prefix_text(),
        requests[1].control_context.prefix_text(),
        "legacy recall must not destabilize the prefix"
    );

    let AgentEvent::MemoryRecalled {
        ids,
        budget,
        block_digest,
        catalog_ids,
        ..
    } = memory_v2_harness::receipt(&first)
    else {
        unreachable!()
    };
    assert!(ids.contains(&id), "legacy recall lost its id: {ids:?}");
    // The legacy path has no Markdown builder receipt; it must keep the old
    // shape rather than inventing catalog or budget claims.
    assert!(catalog_ids.is_empty());
    assert!(block_digest.is_none());
    assert_eq!(budget.total_tokens, 0);
}
