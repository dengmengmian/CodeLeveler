//! Stage 3: the memory context builder's selection and budget.
//!
//! The builder is the only place that decides what memory text reaches a
//! request. These tests exercise it through the executor so scope priority,
//! budgets and degradation are observable in the provider request.

mod memory_v2_harness;

use std::sync::Arc;

use leveler_agent::AgentEvent;
use leveler_agent::coding::policy::ResolvedContextPolicy;
use leveler_memory::ServiceScope;
use memory_v2_harness::{ScriptedRuntime, assistant_text, executor, fact, receipt, run_turn};

#[tokio::test]
async fn the_most_specific_scope_wins_a_shared_identity() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    fact(
        &service,
        ServiceScope::Global,
        "release_channel",
        "发布通道",
        "发布使用 global 通道部署。",
    );
    fact(
        &service,
        ServiceScope::Project,
        "release_channel",
        "发布通道",
        "发布使用 project 通道部署。",
    );
    let local = fact(
        &service,
        ServiceScope::Local,
        "release_channel",
        "发布通道",
        "发布使用 local 通道部署。",
    );
    let runtime = Arc::new(ScriptedRuntime::new(vec![assistant_text("local")]));
    let executor = executor(fixture.workdir(), service, runtime.clone());
    let events = run_turn(&executor, "你好").await;

    let AgentEvent::MemoryRecalled { ids, .. } = receipt(&events) else {
        unreachable!()
    };
    assert_eq!(ids, &vec![local.clone()], "one scope must win the identity");
    let trailing = runtime.requests()[0].control_context.trailing_text();
    assert!(trailing.contains("发布使用 local 通道部署。"), "{trailing}");
    assert!(!trailing.contains("global 通道部署"), "{trailing}");
    assert!(!trailing.contains("project 通道部署"), "{trailing}");
}

#[tokio::test]
async fn the_budget_follows_the_resolved_context_policy() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    for index in 0..12 {
        fact(
            &service,
            ServiceScope::Project,
            &format!("preference_{index}"),
            &format!("偏好 {index}"),
            &format!("偏好内容 {index}：{}", "使用既有方案。".repeat(50)),
        );
    }
    let small = Arc::new(ScriptedRuntime::new(vec![assistant_text("ok")]));
    let default_run = executor(fixture.workdir(), service.clone(), small.clone());
    let events = run_turn(&default_run, "你好").await;
    let AgentEvent::MemoryRecalled { budget, .. } = receipt(&events) else {
        unreachable!()
    };
    assert_eq!(budget.total_tokens, 327, "default pressure threshold");
    assert!(budget.used_tokens <= budget.total_tokens);

    let large = Arc::new(ScriptedRuntime::new(vec![assistant_text("ok")]));
    let policy = ResolvedContextPolicy {
        context_window: 200_000,
        pressure_threshold: 100_000,
        ..ResolvedContextPolicy::default()
    };
    let executor = executor(fixture.workdir(), service, large.clone()).with_context_policy(policy);
    let events = run_turn(&executor, "你好").await;
    let AgentEvent::MemoryRecalled {
        budget, omitted, ..
    } = receipt(&events)
    else {
        unreachable!()
    };
    assert_eq!(budget.total_tokens, 1_200);
    assert!(budget.used_tokens <= budget.total_tokens);
    // Twelve standing facts cannot all fit: omission is explicit, never a
    // silently shortened block.
    assert!(
        omitted.iter().any(|omission| omission.reason == "budget"),
        "{omitted:?}"
    );
}

#[tokio::test]
async fn a_corrupt_index_degrades_explicitly_and_keeps_standing_facts() {
    let fixture = memory_v2_harness::Fixture::new();
    let writer = fixture.service();
    let id = fact(
        &writer,
        ServiceScope::Project,
        "release_channel",
        "发布通道",
        "发布使用蓝色通道部署。",
    );
    drop(writer);
    // Make the index database impossible to open while the Markdown store
    // stays healthy: a directory where the database file belongs. (A merely
    // corrupt database is rebuilt from Markdown by `MemoryIndex::open`, which
    // is the controlled path; this is the degraded one.)
    let index_dir = fixture.project_state.join("index");
    for file in ["fts.sqlite", "fts.sqlite-wal", "fts.sqlite-shm"] {
        let _ = std::fs::remove_file(index_dir.join(file));
    }
    std::fs::create_dir(index_dir.join("fts.sqlite")).unwrap();

    let service = fixture.service();
    let runtime = Arc::new(ScriptedRuntime::new(vec![assistant_text("蓝色")]));
    let executor = executor(fixture.workdir(), service, runtime.clone());
    let events = run_turn(&executor, "发布通道是什么").await;

    let AgentEvent::MemoryRecalled {
        ids,
        degraded,
        omitted,
        ..
    } = receipt(&events)
    else {
        unreachable!()
    };
    assert_eq!(degraded.as_deref(), Some("index_unavailable"));
    assert!(
        omitted
            .iter()
            .any(|omission| omission.reason == "index_stale"),
        "{omitted:?}"
    );
    // Degradation is not an empty success: the current standing fact still
    // comes from Markdown, and the request says the index could not be read.
    let trailing = runtime.requests()[0].control_context.trailing_text();
    assert!(
        ids.contains(&id),
        "standing fallback lost the fact: {ids:?}; tail: {trailing}"
    );
    assert!(trailing.contains("发布使用蓝色通道部署。"), "{trailing}");
}

#[tokio::test]
async fn an_externally_edited_body_is_never_injected() {
    let fixture = memory_v2_harness::Fixture::new();
    let writer = fixture.service();
    let id = fact(
        &writer,
        ServiceScope::Project,
        "release_channel",
        "发布通道",
        "发布使用蓝色通道部署。",
    );
    drop(writer);

    // A person edits the Markdown file outside the trusted write path. The
    // derived index still holds the old row; trust validation must drop it.
    let path = fixture.project_current.join(format!("{id}.md"));
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("发布使用蓝色通道部署。", "发布使用红色通道部署。"),
    )
    .unwrap();

    let service = fixture.service();
    let runtime = Arc::new(ScriptedRuntime::new(vec![assistant_text("没有")]));
    let executor = executor(fixture.workdir(), service, runtime.clone());
    let events = run_turn(&executor, "发布通道是什么").await;
    let AgentEvent::MemoryRecalled { ids, .. } = receipt(&events) else {
        unreachable!()
    };
    assert!(!ids.contains(&id), "untrusted record was recalled: {ids:?}");
    let trailing = runtime.requests()[0].control_context.trailing_text();
    assert!(!trailing.contains("蓝色通道部署"), "{trailing}");
    assert!(!trailing.contains("红色通道部署"), "{trailing}");
}
