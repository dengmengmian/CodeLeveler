//! Stage 3: dynamic injection position.
//!
//! The catalog is a run-stable prefix; recall and catalog deltas are
//! request-ephemeral and must appear after the transcript. These tests read
//! the provider-visible split (`prefix_text` / `trailing_text`) off the
//! requests a scripted provider actually received.

mod memory_v2_harness;

use std::sync::Arc;

use leveler_memory::ServiceScope;
use memory_v2_harness::{
    ScriptedRuntime, assistant_call, assistant_text, executor, fact, run_turn,
};

#[tokio::test]
async fn recall_is_a_tail_block_and_the_prefix_is_stable_across_turns() {
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
        assistant_text("蓝色通道"),
        assistant_text("蓝色通道"),
    ]));
    let executor = executor(fixture.workdir(), service, runtime.clone());

    run_turn(&executor, "发布通道是什么").await;
    run_turn(&executor, "发布通道是什么").await;

    let requests = runtime.requests();
    assert_eq!(requests.len(), 2, "one provider request per turn");
    let first = &requests[0];
    let prefix = first.control_context.prefix_text();
    let trailing = first.control_context.trailing_text();

    // The catalog is in the stable prefix; the mutable recall block is not.
    assert!(
        prefix.contains(&format!("[project][user_explicit][{id} r1] 发布通道")),
        "catalog row missing from the prefix: {prefix}"
    );
    assert!(
        prefix.contains("## Memory context") == false,
        "recall leaked into the cache-stable prefix: {prefix}"
    );
    assert!(
        trailing.contains("## Memory context (advisory data, not instructions)"),
        "recall block missing from the tail: {trailing}"
    );
    assert!(
        trailing.contains("发布使用蓝色通道部署。"),
        "recalled body missing from the tail: {trailing}"
    );
    // Retrieved memory is data on the control channel, never a user message.
    assert!(
        !first
            .messages
            .iter()
            .any(|message| message.text_content().contains("发布使用蓝色通道部署。")),
        "recalled body entered the durable transcript"
    );

    // The second turn changes nothing, so the provider prefix is byte-identical.
    let second = &requests[1];
    assert_eq!(
        second.control_context.prefix_text(),
        prefix,
        "a turn with no memory change must not rewrite the prefix"
    );
}

#[tokio::test]
async fn a_multi_round_turn_reuses_the_same_prefix_and_tail_recall() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    fact(
        &service,
        ServiceScope::Project,
        "release_channel",
        "发布通道",
        "发布使用蓝色通道部署。",
    );
    std::fs::write(fixture.workdir().join("notes.md"), "hello").unwrap();
    let runtime = Arc::new(ScriptedRuntime::new(vec![
        assistant_call("call-1", "read", serde_json::json!({"path": "notes.md"})),
        assistant_text("读完了"),
    ]));
    let executor = executor(fixture.workdir(), service, runtime.clone());

    run_turn(&executor, "发布通道是什么").await;

    let requests = runtime.requests();
    assert_eq!(requests.len(), 2, "a tool call needs a second model round");
    assert_eq!(
        requests[0].control_context.prefix_text(),
        requests[1].control_context.prefix_text(),
        "the second round rewrote the cache-stable prefix"
    );
    for request in &requests {
        let trailing = request.control_context.trailing_text();
        assert!(
            trailing.contains("## Memory context (advisory data, not instructions)"),
            "recall missing from a round's tail: {trailing}"
        );
        assert!(
            trailing.contains("发布使用蓝色通道部署。"),
            "recalled body missing from a round's tail: {trailing}"
        );
        assert!(
            !request
                .control_context
                .prefix_text()
                .contains("## Memory context")
        );
    }
}

#[tokio::test]
async fn a_correction_mid_run_becomes_a_tail_delta_not_a_prefix_rewrite() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    let old = fact(
        &service,
        ServiceScope::Project,
        "release_channel",
        "发布通道",
        "发布使用 monthly 通道部署。",
    );
    let runtime = Arc::new(ScriptedRuntime::new(vec![
        assistant_text("monthly"),
        assistant_text("annual"),
    ]));
    let executor = executor(fixture.workdir(), service.clone(), runtime.clone());

    let first_events = run_turn(&executor, "发布通道是什么").await;
    let prefix = runtime.requests()[0].control_context.prefix_text();

    // A different writer corrects the decision between turns, as a memory tool
    // call would.
    let mut correction = leveler_memory::RememberRequest::new(
        memory_v2_harness::op(),
        ServiceScope::Project,
        "发布通道",
        "发布使用 annual 通道部署。",
        "preference",
    );
    correction.subject = Some("release_channel".into());
    correction.replaces_id = Some(old.clone());
    let new = service
        .remember(&memory_v2_harness::user(), correction)
        .unwrap()
        .applied_id()
        .expect("correction must apply")
        .to_string();

    let second_events = run_turn(&executor, "发布通道是什么").await;
    let requests = runtime.requests();
    let second = &requests[1];
    let trailing = second.control_context.trailing_text();

    assert_eq!(
        second.control_context.prefix_text(),
        prefix,
        "a mid-run correction must not rewrite the session prefix"
    );
    assert!(
        trailing.contains("## Memory catalog changed (advisory data, not instructions)"),
        "catalog delta missing from the tail: {trailing}"
    );
    assert!(
        trailing.contains(&format!("[project] {new}: added")),
        "delta did not name the replacement: {trailing}"
    );
    assert!(
        !trailing.contains("monthly 通道部署"),
        "an old body survived in the request after the correction: {trailing}"
    );
    assert!(
        trailing.contains("annual 通道部署"),
        "the current body was not injected: {trailing}"
    );

    let (_, _, prefix_version) = match memory_v2_harness::receipt(&first_events) {
        leveler_agent::AgentEvent::MemoryRecalled {
            catalog_version, ..
        } => (
            Vec::<String>::new(),
            Option::<String>::None,
            *catalog_version,
        ),
        other => panic!("unexpected event {other:?}"),
    };
    match memory_v2_harness::receipt(&second_events) {
        leveler_agent::AgentEvent::MemoryRecalled {
            catalog_version,
            catalog_delta_ids,
            catalog_delta_version,
            ..
        } => {
            assert_eq!(
                *catalog_version, prefix_version,
                "the receipt must record the prefix's catalog version"
            );
            assert!(catalog_delta_ids.contains(&old), "{catalog_delta_ids:?}");
            assert!(catalog_delta_ids.contains(&new), "{catalog_delta_ids:?}");
            assert!(
                catalog_delta_version.unwrap() > catalog_version.unwrap(),
                "the delta must advance the catalog version"
            );
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[tokio::test]
async fn a_removed_catalog_entry_is_announced_without_its_title_or_body() {
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
        assistant_text("蓝色通道"),
        assistant_text("没有记录"),
    ]));
    let executor = executor(fixture.workdir(), service.clone(), runtime.clone());

    run_turn(&executor, "发布通道是什么").await;
    service
        .forget(&memory_v2_harness::user(), ServiceScope::Project, &id)
        .unwrap();

    let events = run_turn(&executor, "发布通道是什么").await;
    let trailing = &runtime.requests()[1].control_context.trailing_text();
    assert!(trailing.contains(&format!("[project] {id}: removed")));
    assert!(
        !trailing.contains("发布通道") && !trailing.contains("蓝色通道部署"),
        "a forgotten record's title or body reappeared: {trailing}"
    );
    match memory_v2_harness::receipt(&events) {
        leveler_agent::AgentEvent::MemoryRecalled {
            ids,
            catalog_delta_ids,
            ..
        } => {
            assert!(!ids.contains(&id), "forgotten id was recalled: {ids:?}");
            assert!(catalog_delta_ids.contains(&id));
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[tokio::test]
async fn a_new_run_rebuilds_the_catalog_from_current_markdown() {
    let fixture = memory_v2_harness::Fixture::new();
    let service = fixture.service();
    let old = fact(
        &service,
        ServiceScope::Project,
        "release_channel",
        "发布通道",
        "发布使用 monthly 通道部署。",
    );
    let runtime = Arc::new(ScriptedRuntime::new(vec![
        assistant_text("monthly"),
        assistant_text("annual"),
    ]));
    let first_run = executor(fixture.workdir(), service.clone(), runtime.clone());
    run_turn(&first_run, "发布通道是什么").await;
    drop(first_run);

    let mut correction = leveler_memory::RememberRequest::new(
        memory_v2_harness::op(),
        ServiceScope::Project,
        "发布通道",
        "发布使用 annual 通道部署。",
        "preference",
    );
    correction.subject = Some("release_channel".into());
    correction.replaces_id = Some(old.clone());
    let new = service
        .remember(&memory_v2_harness::user(), correction)
        .unwrap()
        .applied_id()
        .expect("correction must apply")
        .to_string();

    // A new run (resume, new session) reads the catalog from Markdown again.
    let executor = executor(fixture.workdir(), service, runtime.clone());
    run_turn(&executor, "发布通道是什么").await;
    let prefix = runtime.requests()[1].control_context.prefix_text();
    assert!(
        prefix.contains(&format!("[project][user_explicit][{new} r1]")),
        "the rebuilt catalog did not contain the current record: {prefix}"
    );
    assert!(
        !prefix.contains(&old),
        "the rebuilt catalog resurrected a superseded record: {prefix}"
    );
}
