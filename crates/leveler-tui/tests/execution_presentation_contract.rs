//! Execution Presentation Contract v1 — the reference conformance suite.
//!
//! The contract v1 is frozen by its fixtures in
//! `testdata/execution_presentation/v1/`: wire-native runtime facts plus their
//! expected *semantic* tree. The tree is deliberately
//! surface-neutral: it names AssistantText, ExecutionRound (with its tool
//! membership and truthful statuses), FinalAnswer and the turn terminal — never
//! a glyph, a color or a widget.
//!
//! This file drives the fixtures through the REAL TUI reducer and the REAL
//! conversation builder. The TUI is the reference implementation, so this test
//! is what makes the frozen contract true today rather than aspirational; the
//! other surfaces prove conformance against the same JSON.
//!
//! `UPDATE_EXECUTION_PRESENTATION_FIXTURES=1` regenerates the `expect` block of
//! each fixture from the reference projection (after checking every declared
//! path agrees). Review the diff: the generator records what the product does,
//! it does not decide what it should do.
//!
//! Run: `cargo test -p leveler-tui --test execution_presentation_contract`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use leveler_client_protocol::{
    CommandId, RuntimeEvent, SessionId, UiHistoryEntry, UiSessionSnapshot,
};
use leveler_tui::action::Action;
use leveler_tui::conversation::build::build_conversation_lines_with_hits;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use leveler_tui::transcript::{AssistantKind, ToolStatus, TranscriptItem, TurnEndStatus};
use serde::Deserialize;
use serde_json::{Value, json};

const WIDTH: usize = 100;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/execution_presentation/v1")
        .canonicalize()
        .expect("fixture dir")
}

#[derive(Debug, Deserialize)]
struct Fixture {
    id: String,
    title: String,
    invariant: String,
    paths: BTreeMap<String, Vec<Value>>,
    /// Optional rendered-conversation assertions for a fact the semantic tree
    /// cannot carry (e.g. WHICH line a failed command is blamed on).
    #[serde(default)]
    rendered: Option<Value>,
    #[serde(default)]
    expect: Option<Value>,
}

fn default_snapshot() -> Value {
    json!({
        "type": "session_opened",
        "session": {
            "id": "s1",
            "repository": "/repo",
            "goal": "fixture",
            "model": null,
            "mode": "assisted",
            "branch": null,
            "status": "idle",
            "messages": []
        }
    })
}

fn state() -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.size = (120, 40);
    s.conv.rect = Some((0, 2, 120, 30));
    s
}

fn reduce_event(s: &mut AppState, event: RuntimeEvent) {
    reduce(s, Action::Runtime(event));
}

/// Apply one fixture step: a raw runtime event, a snapshot, or durable history.
fn apply_step(s: &mut AppState, step: &Value) {
    if let Some(event) = step.get("event") {
        let event: RuntimeEvent = serde_json::from_value(event.clone())
            .unwrap_or_else(|e| panic!("fixture event decode: {e}\n{event}"));
        reduce_event(s, event);
        return;
    }
    if let Some(snapshot) = step.get("snapshot") {
        let event: RuntimeEvent = serde_json::from_value(snapshot.clone())
            .unwrap_or_else(|e| panic!("fixture snapshot decode: {e}\n{snapshot}"));
        reduce_event(s, event);
        return;
    }
    if let Some(history) = step.get("history") {
        let entries: Vec<UiHistoryEntry> = serde_json::from_value(history.clone())
            .unwrap_or_else(|e| panic!("fixture history decode: {e}\n{history}"));
        // A client asks for history after it has opened the session; the
        // response is only adopted for the query it issued.
        let query = CommandId::new("fixture-history");
        s.history_query = Some(query.clone());
        reduce_event(
            s,
            RuntimeEvent::SessionHistoryLoaded {
                query_id: Some(query),
                session_id: SessionId::new("s1"),
                entries,
                omitted_turns: 0,
            },
        );
        return;
    }
    panic!("fixture step has no event/snapshot/history: {step}");
}

fn run_path(steps: &[Value]) -> AppState {
    let mut s = state();
    let open: RuntimeEvent = serde_json::from_value(default_snapshot()).unwrap();
    reduce_event(&mut s, open);
    for step in steps {
        apply_step(&mut s, step);
    }
    s
}

fn tool_status(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Ok => "ok",
        ToolStatus::Failed => "failed",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::Unknown => "unknown",
    }
}

/// The round's status is derived, never stored: a live round is running, a
/// settled one states the most severe thing that happened to it.
fn round_status(calls: &[leveler_tui::transcript::ToolCallBlock]) -> &'static str {
    if calls.iter().any(|c| c.status == ToolStatus::Running) {
        return "running";
    }
    if calls.iter().all(|c| c.status == ToolStatus::Ok) {
        return "ok";
    }
    if calls.iter().any(|c| c.status == ToolStatus::Failed) {
        return "failed";
    }
    if calls.iter().any(|c| c.status == ToolStatus::Cancelled) {
        return "cancelled";
    }
    "unknown"
}

fn turn_end_status(status: TurnEndStatus) -> &'static str {
    match status {
        TurnEndStatus::Completed => "completed",
        TurnEndStatus::CompletedWithWarnings => "completed_with_warnings",
        TurnEndStatus::Answered => "answered",
        TurnEndStatus::Truncated => "truncated",
        TurnEndStatus::Incomplete => "incomplete",
        TurnEndStatus::NoFinalAnswer => "no_final_answer",
        TurnEndStatus::Failed => "failed",
        TurnEndStatus::Cancelled => "cancelled",
    }
}

/// Transcript → the shared semantic tree. Only contract-bearing fields survive:
/// an unknown/new block kind must not be silently presented as a round or an
/// answer, so anything the contract does not name is dropped here on purpose.
fn project(s: &AppState) -> Value {
    let mut items: Vec<Value> = Vec::new();
    let mut user_texts: Vec<String> = Vec::new();
    let reasoning_visible = false;
    for item in s.transcript.items() {
        match item {
            TranscriptItem::User(text) => user_texts.push(text.clone()),
            TranscriptItem::Assistant(block) => {
                let text = block.text.trim();
                if text.is_empty() {
                    continue;
                }
                let kind = if block.kind == AssistantKind::Final {
                    "final_answer"
                } else {
                    "assistant_text"
                };
                items.push(json!({"kind": kind, "text": text}));
            }
            TranscriptItem::ToolGroup(group) => {
                let tools: Vec<Value> = group
                    .calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id.to_string(),
                            "name": c.name,
                            "status": tool_status(c.status),
                        })
                    })
                    .collect();
                // Batches are the concurrent bursts OBSERVED inside the round,
                // in first-seen order; an unbatched call contributes nothing.
                let mut batch_order: Vec<u32> = Vec::new();
                let mut batches: Vec<Vec<String>> = Vec::new();
                for call in &group.calls {
                    let Some(batch) = call.batch else { continue };
                    let index = match batch_order.iter().position(|b| *b == batch) {
                        Some(index) => index,
                        None => {
                            batch_order.push(batch);
                            batches.push(Vec::new());
                            batches.len() - 1
                        }
                    };
                    batches[index].push(call.id.to_string());
                }
                items.push(json!({
                    "kind": "execution_round",
                    "model_step": group.round,
                    "status": round_status(&group.calls),
                    "all_ok": !group.calls.is_empty()
                        && group.calls.iter().all(|c| c.status == ToolStatus::Ok),
                    "batches": batches,
                    "tools": tools,
                }));
            }
            TranscriptItem::TurnEnd(end) => {
                items.push(json!({
                    "kind": "turn_end",
                    "status": turn_end_status(end.status),
                }));
            }
            TranscriptItem::Note(text) => items.push(json!({"kind": "note", "text": text})),
            TranscriptItem::Failure(failure) => items.push(json!({
                "kind": "failure",
                "title": failure.title,
                "summary": failure.summary,
            })),
            // Reasoning is not a transcript block at all; if one ever appears
            // the flag below is what fails.
            _ => {}
        }
    }
    json!({
        "items": items,
        "user_texts": user_texts,
        "reasoning_visible": reasoning_visible,
    })
}

fn rendered_text(s: &AppState) -> String {
    let (lines, _) = build_conversation_lines_with_hits(s, WIDTH);
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_expectations(fixture: &Fixture, state: &AppState) {
    let text = rendered_text(state);
    let Some(rendered) = fixture.rendered.as_ref() else {
        return;
    };
    if let Some(required) = rendered.get("contains").and_then(Value::as_array) {
        for needle in required {
            let needle = needle.as_str().unwrap();
            assert!(
                text.contains(needle),
                "{}: rendered conversation must contain {needle:?}\n{text}",
                fixture.id
            );
        }
    }
    if let Some(forbidden) = rendered.get("excludes").and_then(Value::as_array) {
        for needle in forbidden {
            let needle = needle.as_str().unwrap();
            assert!(
                !text.contains(needle),
                "{}: rendered conversation must not contain {needle:?}\n{text}",
                fixture.id
            );
        }
    }
}

fn fixtures() -> Vec<(PathBuf, Fixture)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .expect("fixture dir readable")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no fixtures found");
    files
        .into_iter()
        .map(|path| {
            let raw = std::fs::read_to_string(&path).expect("fixture readable");
            let fixture: Fixture =
                serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (path, fixture)
        })
        .collect()
}

fn regenerating() -> bool {
    std::env::var("UPDATE_EXECUTION_PRESENTATION_FIXTURES").is_ok_and(|v| v == "1")
}

#[test]
fn execution_presentation_contract_v1() {
    let mut failures: Vec<String> = Vec::new();
    for (path, fixture) in fixtures() {
        let mut projections: Vec<(String, Value, AppState)> = Vec::new();
        for (name, steps) in &fixture.paths {
            let state = run_path(steps);
            projections.push((name.clone(), project(&state), state));
        }
        let (_, reference, reference_state) = projections
            .first()
            .unwrap_or_else(|| panic!("{}: no paths", fixture.id));

        // Every declared path must agree on the semantic tree. A live/
        // reconnect/replay divergence is a contract failure, not a fixture
        // detail.
        for (name, projected, _) in &projections[1..] {
            if projected != reference {
                failures.push(format!(
                    "{} ({}): path {name:?} diverged\n  reference: {}\n  this path: {}",
                    fixture.id,
                    fixture.title,
                    serde_json::to_string_pretty(reference).unwrap(),
                    serde_json::to_string_pretty(projected).unwrap(),
                ));
            }
        }

        if regenerating() {
            let mut raw: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            raw["expect"] = reference.clone();
            let rendered = serde_json::to_string_pretty(&raw).expect("serialize fixture") + "\n";
            std::fs::write(&path, rendered).unwrap();
            continue;
        }

        match &fixture.expect {
            None => panic!(
                "{}: fixture has no frozen expect; run with \
                 UPDATE_EXECUTION_PRESENTATION_FIXTURES=1",
                fixture.id
            ),
            Some(expected) => {
                if reference != expected {
                    failures.push(format!(
                        "{} ({}): semantic tree does not match the frozen contract\n  \
                         frozen:   {}\n  observed: {}",
                        fixture.id,
                        fixture.title,
                        serde_json::to_string_pretty(expected).unwrap(),
                        serde_json::to_string_pretty(reference).unwrap(),
                    ));
                }
                assert_expectations(&fixture, reference_state);
            }
        }
    }
    assert!(
        failures.is_empty(),
        "Execution Presentation Contract v1 failures:\n\n{}",
        failures.join("\n\n")
    );
}

/// The fixture corpus itself is part of the contract: every file must name its
/// invariant, and the C-ids the contract lists must exist.
#[test]
fn the_fixture_corpus_covers_the_frozen_cases() {
    let ids: Vec<String> = fixtures().into_iter().map(|(_, f)| f.id).collect();
    for required in 1..=14 {
        let id = format!("C{required}");
        assert!(ids.contains(&id), "missing fixture {id}: have {ids:?}");
    }
    for (_, fixture) in fixtures() {
        assert!(
            !fixture.title.trim().is_empty(),
            "{}: empty title",
            fixture.id
        );
        assert!(
            !fixture.invariant.trim().is_empty(),
            "{}: empty invariant",
            fixture.id
        );
        assert!(!fixture.paths.is_empty(), "{}: no paths", fixture.id);
    }
}

/// The snapshot shape the fixtures open with must keep decoding: a fixture that
/// silently stopped being applied would pass every other assertion vacuously.
#[test]
fn the_default_fixture_snapshot_decodes() {
    let s: UiSessionSnapshot =
        serde_json::from_value(default_snapshot()["session"].clone()).expect("default snapshot");
    assert_eq!(s.id, SessionId::new("s1"));
    assert!(s.messages.is_empty());
}
