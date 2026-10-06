//! The wire half of the reconnect round contract.
//!
//! `UiActiveToolCall::model_step` is additive: a snapshot taken before the
//! field existed must still decode (as an unknown round, never an invented
//! one), and a runtime that does not know the round must not put a key on the
//! wire. This is what lets an old peer keep its own grouping while a new one
//! restores the real execution round.

use leveler_client_protocol::{
    PermissionProfile, RuntimeEvent, SessionId, ToolCallId, UiActiveToolCall, UiSessionSnapshot,
};

/// The pre-`model_step` shape, exactly as an old runtime wrote it.
fn legacy_tool_json() -> serde_json::Value {
    serde_json::json!({
        "id": "call-1",
        "name": "run_command",
        "arguments": "{}",
    })
}

fn active_tool(id: &str, model_step: Option<u32>) -> UiActiveToolCall {
    UiActiveToolCall {
        id: ToolCallId::new(id),
        name: "run_command".into(),
        arguments: "{}".into(),
        elapsed_ms: 0,
        output_tail: String::new(),
        output_truncated: false,
        model_step,
    }
}

#[test]
fn a_legacy_active_tool_decodes_with_an_unknown_round() {
    let tool: UiActiveToolCall = serde_json::from_value(legacy_tool_json()).expect("legacy decode");
    assert_eq!(tool.model_step, None);
    // The fields that always existed are untouched by the additive one.
    assert_eq!(tool.id, ToolCallId::new("call-1"));
    assert_eq!(tool.name, "run_command");
    assert_eq!(tool.elapsed_ms, 0);
}

#[test]
fn an_unknown_round_stays_off_the_wire() {
    let tool = active_tool("call-1", None);
    let wire = serde_json::to_value(&tool).unwrap();
    assert!(
        wire.get("model_step").is_none(),
        "an unknown round must not be invented on the wire: {wire}"
    );
    assert_eq!(
        serde_json::from_value::<UiActiveToolCall>(wire).unwrap(),
        tool
    );
}

#[test]
fn a_known_round_round_trips() {
    let tool = active_tool("call-1", Some(7));
    let wire = serde_json::to_value(&tool).unwrap();
    assert_eq!(wire["model_step"], 7);
    assert_eq!(
        serde_json::from_value::<UiActiveToolCall>(wire).unwrap(),
        tool
    );
}

#[test]
fn an_explicit_null_round_decodes_as_unknown() {
    // A peer that serializes `Option::None` instead of omitting it is still a
    // legacy peer, not an error.
    let mut json = legacy_tool_json();
    json["model_step"] = serde_json::Value::Null;
    let tool: UiActiveToolCall = serde_json::from_value(json).unwrap();
    assert_eq!(tool.model_step, None);
}

#[test]
fn a_session_snapshot_carries_the_rounds_of_its_running_calls() {
    let snapshot = UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: Some("/repo".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        model: None,
        mode: PermissionProfile::Assisted,
        branch: None,
        status: "running".into(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: vec![
            active_tool("a", Some(3)),
            active_tool("b", Some(3)),
            active_tool("c", None),
        ],
        active_background_tasks: Vec::new(),
        plan: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        thinking: None,
        work_profile: None,
        collaboration: None,
        children: Vec::new(),
    };

    let wire = serde_json::to_value(&snapshot).unwrap();
    let back: UiSessionSnapshot = serde_json::from_value(wire).unwrap();
    assert_eq!(back, snapshot);
    assert_eq!(
        back.active_tools
            .iter()
            .map(|t| t.model_step)
            .collect::<Vec<_>>(),
        vec![Some(3), Some(3), None]
    );
}

#[test]
fn a_legacy_tool_call_started_event_decodes_with_an_unknown_round() {
    // The event that owns the round has always been additive too; a recorded
    // pre-round event must not fabricate one.
    let legacy = serde_json::json!({
        "type": "tool_call_started",
        "id": "call-1",
        "name": "run_command",
        "arguments": "{}",
    });
    let event: RuntimeEvent = serde_json::from_value(legacy).unwrap();
    match event {
        RuntimeEvent::ToolCallStarted {
            model_step,
            parallel,
            ..
        } => {
            assert_eq!(model_step, None);
            assert!(!parallel);
        }
        other => panic!("wrong variant: {other:?}"),
    }

    let modern = serde_json::json!({
        "type": "tool_call_started",
        "id": "call-1",
        "name": "run_command",
        "arguments": "{}",
        "model_step": 4,
    });
    match serde_json::from_value::<RuntimeEvent>(modern).unwrap() {
        RuntimeEvent::ToolCallStarted { model_step, .. } => assert_eq!(model_step, Some(4)),
        other => panic!("wrong variant: {other:?}"),
    }
}
