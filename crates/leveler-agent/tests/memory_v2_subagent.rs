//! Stage 3: sub-agents share memory without opening a second catalog.
//!
//! A child inherits the parent's memory location and lane, so recall works at
//! any depth. The stable prefix catalog is a top-level concern: a child starts
//! a fresh transcript, and it must not pay for a second copy of the catalog.

mod memory_v2_harness;

use std::sync::Arc;

use leveler_memory::ServiceScope;
use memory_v2_harness::{
    ScriptedRuntime, assistant_call, assistant_text, executor, fact, run_turn,
};

#[tokio::test]
async fn a_child_recalls_memory_but_receives_no_catalog_prefix() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    let id = fact(
        &service,
        ServiceScope::Project,
        "release_channel",
        "发布通道",
        "发布使用蓝色通道部署。",
    );
    let runtime = Arc::new(ScriptedRuntime::new(vec![
        assistant_call(
            "call-spawn",
            "spawn_agent",
            serde_json::json!({
                "task": "检查发布通道配置",
                "run_in_background": false
            }),
        ),
        assistant_text("child done"),
        assistant_text("parent done"),
    ]));
    let executor = executor(fixture.workdir(), service, runtime.clone());
    run_turn(&executor, "做点什么").await;

    let requests = runtime.requests();
    assert_eq!(
        requests.len(),
        3,
        "expected parent round, child run, parent round"
    );

    let parent_prefix = requests[0].control_context.prefix_text();
    assert!(
        parent_prefix.contains(&format!("[project][user_explicit][{id} r1]")),
        "the top-level prefix lost its catalog: {parent_prefix}"
    );

    let child = &requests[1];
    assert!(
        child
            .messages
            .iter()
            .any(|message| message.text_content().contains("检查发布通道配置")),
        "second request is not the child run"
    );
    let child_prefix = child.control_context.prefix_text();
    assert!(
        !child_prefix.contains(&format!("[project][user_explicit][{id} r1]")),
        "a child prefix carried the top-level catalog: {child_prefix}"
    );
    assert!(
        !child_prefix.contains("## Memory catalog"),
        "a child prefix carried a catalog section: {child_prefix}"
    );

    // Scope and lane are inherited: the current standing fact still reaches
    // the child as tail data, never as a system instruction.
    let child_trailing = child.control_context.trailing_text();
    assert!(
        child_trailing.contains("发布使用蓝色通道部署。"),
        "the child did not inherit recall: {child_trailing}"
    );
    assert!(
        !child
            .messages
            .iter()
            .any(|message| message.text_content().contains("发布使用蓝色通道部署。")),
        "child recall entered the child transcript"
    );
}
