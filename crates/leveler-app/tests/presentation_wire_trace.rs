//! Boundary evidence, not a daemon/GUI trace: the edit runs against a real
//! filesystem, then its confirmed metadata crosses the typed bridge and JSON.
use leveler_app::event_bridge::EventBridge;
use leveler_client_protocol::RuntimeEvent;
use leveler_engine::EngineEvent;
use leveler_execution::{PermissionProfile, Workspace};
use leveler_tools::{Tool, ToolContext, tools::ApplyPatchTool};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn w2_committed_move_diff_survives_bridge_and_wire_without_preview_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let before = (0..180)
        .map(|i| format!("old-line-{i}\n"))
        .collect::<String>();
    let after = (0..180)
        .map(|i| format!("new-line-{i}\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("before.txt"), &before).unwrap();
    let patch = format!(
        "*** Begin Patch\n*** Update File: before.txt\n*** Move to: after.txt\n@@\n{}{}*** End Patch",
        before
            .lines()
            .map(|l| format!("-{l}\n"))
            .collect::<String>(),
        after.lines().map(|l| format!("+{l}\n")).collect::<String>(),
    );
    let output = ApplyPatchTool
        .execute(
            serde_json::json!({"patch": patch}),
            ToolContext::new(
                Workspace::new(dir.path()).unwrap(),
                PermissionProfile::FullAccess,
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.content);
    assert!(!dir.path().join("before.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("after.txt")).unwrap(),
        after
    );
    let confirmed = output.metadata["applied_diff"].as_str().unwrap();
    assert!(confirmed.starts_with("--- a/before.txt\n+++ b/after.txt\n"));
    assert!(confirmed.contains("+new-line-179\n"));
    assert!(confirmed.len() > 4096);
    let (tx, mut rx) = broadcast::channel(8);
    let mut bridge = EventBridge::new(tx);
    bridge.forward(EngineEvent::ToolCallStarted {
        call_id: "confirmed-move-1".into(),
        name: "apply_patch".into(),
        arguments: "untrusted request is not the confirmed diff".into(),
        parallel: false,
        risk: None,
        agent_id: None,
        model_step: Some(2),
    });
    bridge.forward(EngineEvent::ToolCallFinished {
        call_id: "confirmed-move-1".into(),
        name: "apply_patch".into(),
        is_error: false,
        preview: "short preview".into(),
        agent_id: None,
        applied_diff: Some(confirmed.into()),
        exit_code: None,
        stop: None,
    });
    let started = rx.try_recv().unwrap();
    assert!(matches!(
        started,
        RuntimeEvent::ToolCallStarted {
            model_step: Some(2),
            ..
        }
    ));
    let finished = rx.try_recv().unwrap();
    let wire = serde_json::to_vec(&finished).unwrap();
    let decoded: RuntimeEvent = serde_json::from_slice(&wire).unwrap();
    assert!(
        matches!(decoded, RuntimeEvent::ToolCallCompleted { id, applied_diff: Some(diff), preview, ok: true, .. }
        if id.as_str() == "confirmed-move-1" && diff == confirmed && preview == "short preview")
    );
}

fn wire_projection(events: Vec<EngineEvent>) -> Vec<RuntimeEvent> {
    let (tx, mut rx) = broadcast::channel(64);
    let mut bridge = EventBridge::new(tx);
    for event in events {
        // Durable engine payloads cross the same serde boundary used on replay.
        let decoded = serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();
        bridge.forward(decoded);
    }
    let mut result = Vec::new();
    while let Ok(event) = rx.try_recv() {
        result.push(serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap());
    }
    result
}

#[test]
fn w1_completed_thought_wire_is_equivalent_in_live_and_durable_replay() {
    let durable = EngineEvent::AssistantMessage {
        text: "answer".into(),
        reasoning: vec![leveler_model::ReasoningSegment {
            text: "measured thought".into(),
            duration_ms: 321,
        }],
    };
    let live = wire_projection(vec![
        EngineEvent::ReasoningStarted,
        EngineEvent::ReasoningDelta {
            text: "measured thought".into(),
        },
        EngineEvent::ReasoningCompleted { elapsed_ms: 321 },
        durable.clone(),
    ]);
    let replay = wire_projection(vec![durable]);
    let thoughts = |events: Vec<RuntimeEvent>| {
        events
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::ReasoningStarted
                        | RuntimeEvent::ReasoningDelta { .. }
                        | RuntimeEvent::ReasoningCompleted { .. }
                )
            })
            .collect::<Vec<_>>()
    };
    let expected = vec![
        RuntimeEvent::ReasoningStarted,
        RuntimeEvent::ReasoningDelta {
            delta: "measured thought".into(),
        },
        RuntimeEvent::ReasoningCompleted { elapsed_ms: 321 },
    ];
    assert_eq!(thoughts(live), expected);
    assert_eq!(thoughts(replay), expected);
}

#[test]
fn w3_wire_preserves_cross_round_tool_identity_and_error_boundary() {
    let started = |id: &str, step| EngineEvent::ToolCallStarted {
        call_id: id.into(),
        name: "read_file".into(),
        arguments: "{}".into(),
        parallel: false,
        risk: None,
        agent_id: None,
        model_step: Some(step),
    };
    let finished = |id: &str, error| EngineEvent::ToolCallFinished {
        call_id: id.into(),
        name: "read_file".into(),
        is_error: error,
        preview: "receipt".into(),
        agent_id: None,
        applied_diff: None,
        exit_code: None,
        stop: None,
    };
    let events = wire_projection(vec![
        started("read-1", 1),
        finished("read-1", false),
        started("read-2", 2),
        finished("read-2", true),
        started("read-3", 3),
        finished("read-3", false),
    ]);
    let starts = events
        .iter()
        .filter_map(|event| {
            if let RuntimeEvent::ToolCallStarted { id, model_step, .. } = event {
                Some((id.as_str(), *model_step))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        starts,
        vec![
            ("read-1", Some(1)),
            ("read-2", Some(2)),
            ("read-3", Some(3))
        ]
    );
    let finishes = events
        .iter()
        .filter_map(|event| {
            if let RuntimeEvent::ToolCallCompleted {
                id,
                ok,
                applied_diff,
                ..
            } = event
            {
                Some((id.as_str(), *ok, applied_diff.is_none()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        finishes,
        vec![
            ("read-1", true, true),
            ("read-2", false, true),
            ("read-3", true, true)
        ]
    );
}

#[test]
fn w5_w6_authoritative_cancel_freezes_wire_without_a_false_final() {
    let events = wire_projection(vec![
        EngineEvent::ReasoningStarted,
        EngineEvent::ReasoningDelta {
            text: "interrupted thought".into(),
        },
        EngineEvent::TaskFinished {
            outcome: leveler_lifecycle::TaskOutcome::Cancelled,
            reason: None,
            stop: None,
            failure: None,
            warnings: vec![],
        },
        EngineEvent::ReasoningCompleted { elapsed_ms: 999 },
        EngineEvent::AssistantMessage {
            text: "late false success".into(),
            reasoning: vec![],
        },
    ]);
    assert_eq!(
        events,
        vec![
            RuntimeEvent::ReasoningStarted,
            RuntimeEvent::ReasoningDelta {
                delta: "interrupted thought".into()
            },
            RuntimeEvent::TaskCancelled
        ]
    );
}

#[tokio::test]
async fn w2_failed_patch_has_no_confirmed_diff_and_leaves_files_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "actual\n").unwrap();
    let output = ApplyPatchTool.execute(
        serde_json::json!({"patch": "*** Begin Patch\n*** Update File: file.txt\n@@\n-invented\n+claimed\n*** End Patch"}),
        ToolContext::new(Workspace::new(dir.path()).unwrap(), PermissionProfile::FullAccess),
        CancellationToken::new(),
    ).await.unwrap();
    assert!(output.is_error);
    assert!(output.metadata.get("applied_diff").is_none());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "actual\n"
    );
}

#[test]
fn w7_compaction_preserves_the_durable_thought_on_wire_replay() {
    let events = wire_projection(vec![
        EngineEvent::AssistantMessage {
            text: "prior answer".into(),
            reasoning: vec![leveler_model::ReasoningSegment {
                text: "prior measured thought".into(),
                duration_ms: 77,
            }],
        },
        EngineEvent::Compacted { from: 100, to: 40 },
        EngineEvent::AssistantMessage {
            text: "continued answer".into(),
            reasoning: vec![],
        },
    ]);
    assert!(events.contains(&RuntimeEvent::ReasoningDelta {
        delta: "prior measured thought".into()
    }));
    assert!(events.contains(&RuntimeEvent::ReasoningCompleted { elapsed_ms: 77 }));
    assert!(events.contains(&RuntimeEvent::ContextCompacted { from: 100, to: 40 }));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::ReasoningCompleted { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn w2_large_confirmed_edit_is_not_silently_removed_by_a_diff_budget() {
    let dir = tempfile::tempdir().unwrap();
    let content = (0..6000)
        .map(|i| format!("confirmed-line-{i}\n"))
        .collect::<String>();
    let output = leveler_tools::tools::WriteFileTool
        .execute(
            serde_json::json!({"path": "large.txt", "content": content}),
            ToolContext::new(
                Workspace::new(dir.path()).unwrap(),
                PermissionProfile::FullAccess,
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("large.txt")).unwrap(),
        content
    );
    let confirmed = output.metadata["applied_diff"]
        .as_str()
        .expect("a confirmed file change must retain its entire diff");
    assert!(confirmed.len() > 65536);
    assert!(confirmed.ends_with("+confirmed-line-5999\n"));
    let events = wire_projection(vec![EngineEvent::ToolCallFinished {
        call_id: "large-confirmed-edit".into(),
        name: "write_file".into(),
        is_error: false,
        preview: "short receipt".into(),
        agent_id: None,
        applied_diff: Some(confirmed.into()),
        exit_code: None,
        stop: None,
    }]);
    assert!(events.iter().any(|event| matches!(event, RuntimeEvent::ToolCallCompleted { applied_diff: Some(diff), .. } if diff == confirmed)));
}
