//! Reconnect keeps the execution round identity of a running tool.
//!
//! The reconnect snapshot is the only source a reconnecting client has for
//! what is still running, so it must state the round those calls belong to
//! instead of forcing the client to re-derive one. These cases drive the real
//! reducer and the real conversation builder: the geometry they assert is the
//! geometry the terminal paints.
//!
//! Contract (Release Gate `reconnect_round_identity`):
//!   C1  one running call's round survives live → snapshot → restore → settle
//!   C2  calls one model response requested stay ONE round after reconnect
//!   C3  an active round is never welded onto an older one
//!   C4  live, reconnect and replay state the same round
//!   C5  running → reconnect → completed keeps the head row and child column
//!   C6  a legacy snapshot without the field stays on the fallback, safely

use leveler_client_protocol::{
    PermissionProfile, RuntimeEvent, SessionId, ToolCallId, UiActiveToolCall, UiSessionSnapshot,
};
use leveler_tui::action::Action;
use leveler_tui::conversation::build::build_conversation_lines_with_hits;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use leveler_tui::transcript::TranscriptItem;

const W: usize = 100;
const CMD: &str = r#"{"cmd":"sleep 60"}"#;

fn idle_snapshot() -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: Some("/repo".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        model: None,
        mode: PermissionProfile::Assisted,
        branch: None,
        status: "idle".into(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
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
    }
}

fn state() -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".into(),
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
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: idle_snapshot(),
        }),
    );
    s
}

fn reconnect(s: &mut AppState, tools: Vec<UiActiveToolCall>) {
    let mut snap = idle_snapshot();
    snap.status = "running".into();
    snap.active_tools = tools;
    reduce(
        s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
}

fn active_tool(
    id: &str,
    name: &str,
    arguments: &str,
    elapsed_ms: u64,
    model_step: Option<u32>,
) -> UiActiveToolCall {
    UiActiveToolCall {
        id: ToolCallId::new(id),
        name: name.into(),
        arguments: arguments.into(),
        elapsed_ms,
        output_tail: String::new(),
        output_truncated: false,
        model_step,
        // These fixtures describe the round; the answer classification is the
        // caller's, and the reconnect tests below pass it through `start`.
        answer_effect: None,
    }
}

fn start(s: &mut AppState, id: &str, name: &str, arguments: &str, model_step: Option<u32>) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: name.into(),
            arguments: arguments.into(),
            parallel: false,
            model_step,
            // These fixtures are about the execution round; the tools they use
            // (reads, greps, shell runs) all act on the answer.
            answer_effect: None,
        }),
    );
}

fn complete(s: &mut AppState, id: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new(id),
            ok: true,
            preview: "done\n".into(),
            duration_ms: 60_000,
            applied_diff: None,
            exit_code: Some(0),
            stop: None,
        }),
    );
}

fn lines(s: &AppState) -> Vec<String> {
    build_conversation_lines_with_hits(s, W)
        .0
        .into_iter()
        .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
        .collect()
}

/// `(round, tool names)` for each tool group, in transcript order.
fn rounds(s: &AppState) -> Vec<(Option<u32>, Vec<String>)> {
    s.transcript
        .items()
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::ToolGroup(group) => Some((
                group.round,
                group.calls.iter().map(|c| c.name.clone()).collect(),
            )),
            _ => None,
        })
        .collect()
}

fn row(lines: &[String], needle: &str) -> (usize, String) {
    lines
        .iter()
        .enumerate()
        .find(|(_, l)| l.contains(needle))
        .map(|(i, l)| (i, l.clone()))
        .unwrap_or_else(|| panic!("no row with {needle:?}: {lines:?}"))
}

fn indent(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn count_rows(lines: &[String], needle: &str) -> usize {
    lines.iter().filter(|l| l.contains(needle)).count()
}

/// C1 — one running call, and the round it was requested in, survive a
/// disconnect and reconnect, then close in place.
#[test]
fn c1_a_running_calls_round_survives_the_reconnect() {
    let mut live = state();
    live.transcript.push_user("验证续期".into());
    start(&mut live, "c1", "shell_command", CMD, Some(3));

    let mut seen = state();
    seen.elapsed_secs = 5;
    reconnect(
        &mut seen,
        vec![active_tool("c1", "shell_command", CMD, 2_000, Some(3))],
    );

    let expected = vec![(Some(3), vec!["shell_command".to_string()])];
    assert_eq!(rounds(&live), expected, "live states the round");
    assert_eq!(
        rounds(&seen),
        expected,
        "the reconnect restores the stated round, never a re-derived one"
    );

    // The clock was back-dated, not restarted at zero.
    assert!(row(&lines(&seen), "sleep 60 · 2s").1.contains("2s"));

    complete(&mut seen, "c1");
    assert_eq!(
        rounds(&seen),
        expected,
        "the settled round is the same round"
    );
    assert!(
        lines(&seen).iter().any(|l| l.contains("完成 1 项")),
        "{:?}",
        lines(&seen)
    );
}

/// C2 — several calls one model response requested reconnect as ONE round,
/// however different the tools are.
#[test]
fn c2_one_model_response_stays_one_round() {
    let mut seen = state();
    reconnect(
        &mut seen,
        vec![
            active_tool("a", "read_file", r#"{"path":"README.md"}"#, 1_000, Some(2)),
            active_tool("b", "grep", r#"{"pattern":"TaskStatus"}"#, 1_000, Some(2)),
            active_tool("c", "shell_command", CMD, 1_000, Some(2)),
        ],
    );

    assert_eq!(
        rounds(&seen),
        vec![(
            Some(2),
            vec![
                "read_file".to_string(),
                "grep".to_string(),
                "shell_command".to_string()
            ]
        )],
        "one model response is one round: {:?}",
        rounds(&seen)
    );
    assert!(
        lines(&seen).iter().any(|l| l.contains("正在执行 3 项操作")),
        "{:?}",
        lines(&seen)
    );
}

/// C3 — rounds the snapshot states separately never merge into one group.
#[test]
fn c3_the_active_round_is_never_welded_onto_an_older_one() {
    let mut seen = state();
    reconnect(
        &mut seen,
        vec![
            active_tool("late", "shell_command", CMD, 6_000, Some(2)),
            active_tool("now", "read_file", r#"{"path":"README.md"}"#, 500, Some(3)),
        ],
    );

    assert_eq!(
        rounds(&seen),
        vec![
            (Some(2), vec!["shell_command".to_string()]),
            (Some(3), vec!["read_file".to_string()]),
        ],
        "two stated rounds stay two rounds: {:?}",
        rounds(&seen)
    );
}

/// C4 — the live stream, the reconnect snapshot and the replayed durable events
/// state the same round for the same session.
#[test]
fn c4_live_reconnect_and_replay_state_the_same_round() {
    let calls = [
        ("a", "read_file", r#"{"path":"a.md"}"#),
        ("b", "grep", r#"{"pattern":"x"}"#),
    ];

    let mut live = state();
    for (id, name, arguments) in calls {
        start(&mut live, id, name, arguments, Some(4));
    }

    let mut seen = state();
    reconnect(
        &mut seen,
        calls
            .iter()
            .map(|(id, name, arguments)| active_tool(id, name, arguments, 1_000, Some(4)))
            .collect(),
    );

    let mut replay = state();
    for (id, name, arguments) in calls {
        start(&mut replay, id, name, arguments, Some(4));
        complete(&mut replay, id);
    }
    reduce(&mut replay, Action::Runtime(RuntimeEvent::TurnAnswered));

    assert_eq!(rounds(&live), rounds(&seen), "live vs reconnect");
    assert_eq!(rounds(&live), rounds(&replay), "live vs replay");
}

/// C5 — running → reconnect → running → completed never moves the head row or
/// the child column, and never duplicates (or drops) the ToolRow.
#[test]
fn c5_reconnect_keeps_the_running_geometry_until_completion() {
    let mut live = state();
    start(&mut live, "c1", "shell_command", CMD, Some(6));

    let mut seen = state();
    seen.elapsed_secs = 30;
    reconnect(
        &mut seen,
        vec![active_tool("c1", "shell_command", CMD, 2_000, Some(6))],
    );

    // Running: the live frame and the reconnected frame are the same shape.
    let (live_head_at, live_head) = row(&lines(&live), "正在执行 1 项");
    let (seen_head_at, seen_head) = row(&lines(&seen), "正在执行 1 项");
    assert_eq!(live_head, seen_head, "same head text");
    assert_eq!(live_head_at, seen_head_at, "same head row");
    assert_eq!(
        indent(&row(&lines(&live), "sleep 60").1),
        indent(&row(&lines(&seen), "sleep 60").1),
        "same child column"
    );
    assert_eq!(count_rows(&lines(&seen), "sleep 60"), 1, "one ToolRow");

    // Completed: the SAME rows, rewritten.
    complete(&mut seen, "c1");
    let (done_head_at, done_head) = row(&lines(&seen), "完成 1 项");
    assert_eq!(
        done_head_at,
        seen_head_at,
        "closing must not insert a row above the head: {:?}",
        lines(&seen)
    );
    assert_eq!(indent(&done_head), indent(&seen_head), "head stays put");
    assert_eq!(
        indent(&row(&lines(&seen), "sleep 60").1),
        indent(&row(&lines(&live), "sleep 60").1),
        "and the child column does not re-indent"
    );
    assert_eq!(
        count_rows(&lines(&seen), "sleep 60"),
        1,
        "still one ToolRow"
    );
    assert_eq!(
        rounds(&seen),
        vec![(Some(6), vec!["shell_command".to_string()])]
    );
}

/// C6 — a snapshot from before the field existed still restores its calls and
/// stays on the legacy grouping; nothing panics or is dropped.
#[test]
fn c6_a_legacy_snapshot_without_a_round_falls_back_safely() {
    let mut s = state();
    // The real legacy wire shape: no `model_step` key at all.
    let legacy = serde_json::json!({
        "id": "c1",
        "name": "shell_command",
        "arguments": CMD,
        "elapsed_ms": 3_000,
    });
    let tool: UiActiveToolCall = serde_json::from_value(legacy).expect("legacy decode");
    assert_eq!(tool.model_step, None, "an absent round decodes as unknown");

    let mut snap = idle_snapshot();
    snap.status = "running".into();
    snap.active_tools = vec![tool];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );

    assert_eq!(s.transcript.tool_calls().len(), 1, "the call is not lost");
    assert_eq!(
        rounds(&s),
        vec![(None, vec!["shell_command".to_string()])],
        "an absent round stays absent instead of being invented"
    );
    assert!(
        lines(&s).iter().any(|l| l.contains("sleep 60 · 3s")),
        "{:?}",
        lines(&s)
    );

    // A later, modern snapshot states its own rounds and carries no fallback over.
    reconnect(
        &mut s,
        vec![active_tool("c2", "shell_command", CMD, 1_000, Some(9))],
    );
    assert_eq!(
        rounds(&s),
        vec![(Some(9), vec!["shell_command".to_string()])],
        "each snapshot states its own rounds: {:?}",
        rounds(&s)
    );
}

/// R6 — a completed round is not re-minted by a resume, and the durable record
/// shows the answer exactly once.
///
/// Tool rows are transient by design: the durable record of a finished turn is
/// its transcript messages. What a resume must never do is invent a round out
/// of an empty active list, or duplicate the answer the live client already has.
#[test]
fn r6_resume_after_completion_mints_no_orphan_round() {
    let mut live = state();
    live.transcript.push_user("验证续期".into());
    start(&mut live, "c1", "shell_command", CMD, Some(5));
    complete(&mut live, "c1");
    reduce(&mut live, Action::Runtime(RuntimeEvent::TurnAnswered));

    assert_eq!(
        rounds(&live),
        vec![(Some(5), vec!["shell_command".to_string()])],
        "the live round settles in place: {:?}",
        rounds(&live)
    );
    assert_eq!(
        count_rows(&lines(&live), "sleep 60"),
        1,
        "one ToolRow, not two: {:?}",
        lines(&live)
    );

    // Resume: a snapshot of the finished session. Active tools are empty, so no
    // round may be invented — and applying the same snapshot twice is
    // idempotent, never a second set of rows.
    let mut snap = idle_snapshot();
    snap.status = "idle".into();
    let mut resumed = state();
    for _ in 0..2 {
        reduce(
            &mut resumed,
            Action::Runtime(RuntimeEvent::SessionOpened {
                session: snap.clone(),
            }),
        );
    }
    assert!(
        rounds(&resumed).is_empty(),
        "no orphan round: {:?}",
        rounds(&resumed)
    );
    assert_eq!(resumed.transcript.tool_calls().len(), 0);
    assert_eq!(count_rows(&lines(&resumed), "sleep 60"), 0);
}
