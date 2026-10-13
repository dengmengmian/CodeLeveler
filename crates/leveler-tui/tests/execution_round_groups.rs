//! Execution Round grouping, driven through the REAL reducer and the REAL
//! conversation builder.
//!
//! The runtime stamps every tool call with the model step that requested it.
//! These tests assert the visible consequence: one model response is one
//! `ToolGroup`, the next response is a new one, and the round reads as a
//! lightweight tool node (`› 执行命令 · 完成 3 项`) instead of one giant group
//! or a fabricated stage. Geometry is asserted from first call to close, and
//! "all ok" is never claimed over a call that did not succeed.

use leveler_client_protocol::{
    MessageId, RuntimeEvent, SessionId, ToolCallId, UiMessage, UiRole, UiSessionSnapshot,
};
use leveler_tui::action::Action;
use leveler_tui::conversation::build::build_conversation_lines_with_hits;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

const W: usize = 100;

fn opened() -> AppState {
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
    let snap = UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: Some("~/x".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
        mode: leveler_client_protocol::PermissionProfile::Assisted,
        branch: Some("main".into()),
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
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    s
}

fn lines(s: &AppState) -> Vec<String> {
    build_conversation_lines_with_hits(s, W)
        .0
        .into_iter()
        .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
        .collect()
}

fn start(s: &mut AppState, id: &str, program: &str, args: &str, model_step: u32) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: "run_command".into(),
            arguments: format!(r#"{{"program":"{program}","args":[{args}]}}"#),
            parallel: false,
            model_step: Some(model_step),
            answer_effect: None,
        }),
    );
}

/// A read call in `model_step`. Stops short of completion so a caller can
/// drive several calls into one open (running) round.
fn start_read(s: &mut AppState, id: &str, path: &str, model_step: u32) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: "read_file".into(),
            arguments: format!(r#"{{"path":"{path}"}}"#),
            parallel: false,
            model_step: Some(model_step),
            answer_effect: None,
        }),
    );
}

/// A completed read call in `model_step`, so a mixed round is exercised.
fn read_in_round(s: &mut AppState, id: &str, path: &str, model_step: u32) {
    start_read(s, id, path, model_step);
    finish(s, id, true);
}

/// A search call in `model_step`. Stops short of completion so a caller can
/// drive several calls into one open (running) round.
fn start_search(s: &mut AppState, id: &str, pattern: &str, model_step: u32) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: "grep".into(),
            arguments: format!(r#"{{"pattern":"{pattern}"}}"#),
            parallel: false,
            model_step: Some(model_step),
            answer_effect: None,
        }),
    );
}

/// A completed search call in `model_step`.
fn search_in_round(s: &mut AppState, id: &str, pattern: &str, model_step: u32) {
    start_search(s, id, pattern, model_step);
    finish(s, id, true);
}

fn finish(s: &mut AppState, id: &str, ok: bool) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new(id),
            ok,
            preview: if ok { "ok".into() } else { "boom".into() },
            duration_ms: 120,
            applied_diff: None,
        }),
    );
}

/// Fail a call because its process was stopped and its tree confirmed gone.
fn cancel(s: &mut AppState, id: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: Some(leveler_client_protocol::UiCommandStop::Confirmed),
            id: ToolCallId::new(id),
            ok: false,
            preview: String::new(),
            duration_ms: 700,
            applied_diff: None,
        }),
    );
}

fn user(s: &mut AppState, text: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: text.into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
}

fn next_assistant(s: &mut AppState, id: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new(id),
        }),
    );
}

/// The row index of the round's own head (`› 执行命令 · …`).
fn round_head_rows(lines: &[String]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim_start().starts_with('\u{203a}') && l.contains("执行命令"))
        .map(|(i, _)| i)
        .collect()
}

fn recorded_rounds(s: &AppState) -> Vec<(Option<u32>, usize)> {
    s.transcript
        .items()
        .iter()
        .filter_map(|item| match item {
            leveler_tui::transcript::TranscriptItem::ToolGroup(group) => {
                Some((group.round, group.calls.len()))
            }
            _ => None,
        })
        .collect()
}

fn open_latest(s: &mut AppState) {
    reduce(
        s,
        Action::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
}

#[test]
fn prose_free_rounds_share_one_visible_receipt_without_losing_round_identity() {
    let mut s = opened();
    user(&mut s, "检查两个结果");
    read_in_round(&mut s, "r1", "first.rs", 0);
    search_in_round(&mut s, "g1", "first", 0);
    start_read(&mut s, "r2", "next.rs", 1);
    assert_eq!(recorded_rounds(&s), vec![(Some(0), 2), (Some(1), 1)]);
    let live = lines(&s);
    assert!(
        live.iter().any(|line| line.contains("读取 2 次")
            && line.contains("搜索 1 次")
            && line.contains("正在执行 1 项")),
        "{live:?}"
    );
    assert!(!live.iter().any(|line| line.contains("next.rs")));
    finish(&mut s, "r2", true);
    search_in_round(&mut s, "g2", "next", 1);
    next_assistant(&mut s, "answer");
    let settled = lines(&s);
    assert_eq!(
        settled
            .iter()
            .filter(|line| line.contains("读取 2 次") && line.contains("搜索 2 次"))
            .count(),
        1,
        "{settled:?}"
    );
    assert_eq!(recorded_rounds(&s), vec![(Some(0), 2), (Some(1), 2)]);
    open_latest(&mut s);
    let expanded = lines(&s);
    assert!(expanded.iter().any(|line| line.contains("next.rs")));
    assert!(expanded.iter().any(|line| line.contains("first.rs")));
    open_latest(&mut s);
    assert_eq!(lines(&s), settled);
}

#[test]
fn progress_prose_splits_visible_phases_and_ctrl_o_opens_only_the_latest() {
    let mut s = opened();
    read_in_round(&mut s, "before", "before.rs", 0);
    let narration = MessageId::new("progress");
    next_assistant(&mut s, "progress");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: narration.clone(),
            delta: "发现了入口，接下来检查实现。".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: narration,
        }),
    );
    read_in_round(&mut s, "after1", "after-one.rs", 1);
    read_in_round(&mut s, "after2", "after-two.rs", 2);
    let folded = lines(&s);
    assert_eq!(
        folded.iter().filter(|line| line.contains("▸ 读取")).count(),
        2,
        "{folded:?}"
    );
    assert!(folded.iter().any(|line| line.contains("读取 1 次")));
    assert!(folded.iter().any(|line| line.contains("读取 2 次")));
    assert_eq!(
        recorded_rounds(&s),
        vec![(Some(0), 1), (Some(1), 1), (Some(2), 1)]
    );
    open_latest(&mut s);
    let expanded = lines(&s);
    assert!(!expanded.iter().any(|line| line.contains("before.rs")));
    assert!(expanded.iter().any(|line| line.contains("after-one.rs")));
    assert!(expanded.iter().any(|line| line.contains("after-two.rs")));
}

/// Homogeneous shell work accumulates a command count in the phase receipt.
#[test]
fn a_homogeneous_shell_round_accumulates_command_count() {
    let mut s = opened();
    next_assistant(&mut s, "a1");
    start(&mut s, "c1", "cargo", r#""check""#, 1);
    finish(&mut s, "c1", true);
    start(&mut s, "c2", "cargo", r#""test""#, 1);
    finish(&mut s, "c2", true);

    let t = lines(&s);
    let head = t
        .iter()
        .find(|l| l.contains("执行了 2 个命令"))
        .unwrap_or_else(|| panic!("the shell round names the tool: {t:?}"));
    assert!(head.contains("2 个命令"), "it counts both calls: {head:?}");
    assert!(
        !head.contains("多个工具") && !head.contains("项操作"),
        "a same-tool round is not the mixed fallback: {head:?}"
    );
}

/// B — three reads in one round read as ONE compact receipt (`读取 3 次`),
/// not a row per file and not a per-tool count line.
#[test]
fn a_homogeneous_read_round_reads_as_one_receipt() {
    let mut s = opened();
    next_assistant(&mut s, "a1");
    read_in_round(&mut s, "r1", "src/a.rs", 1);
    read_in_round(&mut s, "r2", "src/b.rs", 1);
    read_in_round(&mut s, "r3", "src/c.rs", 1);

    let t = lines(&s);
    let head = t
        .iter()
        .find(|l| l.contains("读取") && l.contains("3 次"))
        .unwrap_or_else(|| panic!("the read round is a receipt: {t:?}"));
    assert!(
        !head.contains("完成 3 项"),
        "it is not a run head: {head:?}"
    );
    assert!(
        !t.iter().any(|l| l.contains("src/a.rs")),
        "no per-file waterfall: {t:?}"
    );
}

/// C — two searches in one round read as `搜索 2 次`.
#[test]
fn a_homogeneous_search_round_reads_as_one_receipt() {
    let mut s = opened();
    next_assistant(&mut s, "a1");
    search_in_round(&mut s, "g1", "fn main", 1);
    search_in_round(&mut s, "g2", "fn run", 1);

    let t = lines(&s);
    let head = t
        .iter()
        .find(|l| l.contains("搜索") && l.contains("2 次"))
        .unwrap_or_else(|| panic!("the search round is a receipt: {t:?}"));
    assert!(
        !head.contains("完成 2 项"),
        "it is not a run head: {head:?}"
    );
}

/// A mixed round (a command and a read) still reads as one lightweight node:
/// it counts its operations instead of naming an abstract "multiple tools",
/// and never claims all-ok over a call that did not succeed.
#[test]
fn a_mixed_round_still_reads_as_one_node() {
    let mut s = opened();
    next_assistant(&mut s, "a1");
    start(&mut s, "c1", "cargo", r#""check""#, 5);
    finish(&mut s, "c1", true);
    read_in_round(&mut s, "r1", "src/lib.rs", 5);
    next_assistant(&mut s, "a2");

    let t = lines(&s);
    let head = t
        .iter()
        .find(|l| l.contains("读取 1 次") && l.contains("1 个命令"))
        .unwrap_or_else(|| panic!("the mixed round counts its operations: {t:?}"));
    assert!(
        !head.contains("多个工具"),
        "the abstract 'multiple tools' label is gone: {head:?}"
    );
    assert!(
        !t.iter()
            .any(|l| l.contains("src/lib.rs") || l.contains("cargo check")),
        "member targets start folded: {head:?}"
    );
}

/// E — a mixed round with a failure counts every operation and still states
/// the failure; nothing is hidden because one of them failed.
#[test]
fn a_mixed_round_with_a_failure_counts_every_operation() {
    let mut s = opened();
    next_assistant(&mut s, "a1");
    start(&mut s, "c1", "cargo", r#""check""#, 1);
    finish(&mut s, "c1", true);
    start_read(&mut s, "r1", "src/lib.rs", 1);
    finish(&mut s, "r1", false);
    search_in_round(&mut s, "g1", "fn main", 1);
    next_assistant(&mut s, "a2");

    let t = lines(&s);
    let head = t
        .iter()
        .find(|l| l.contains("读取 1 次") && l.contains("搜索 1 次") && l.contains("1 个命令"))
        .unwrap_or_else(|| panic!("the mixed round counts every operation: {t:?}"));
    assert!(
        head.contains("1 个失败"),
        "the failure still rides the head: {head:?}"
    );
    assert!(
        !head.contains("多个工具"),
        "the abstract label is gone: {head:?}"
    );
    assert!(!t.iter().any(|l| l.contains("src/lib.rs")));
    open_latest(&mut s);
    assert!(
        lines(&s).iter().any(|l| l.contains("src/lib.rs")),
        "the failed target is reachable by expanding"
    );
}

/// F — while a mixed round is in flight its head says what is running and
/// counts the operations, and states no outcome at all.
#[test]
fn a_running_mixed_round_counts_operations_without_an_outcome() {
    let mut s = opened();
    next_assistant(&mut s, "a1");
    start(&mut s, "c1", "cargo", r#""check""#, 1);
    start_read(&mut s, "r1", "src/lib.rs", 1);

    let t = lines(&s);
    let head = t
        .iter()
        .find(|l| l.contains("正在执行 2 项"))
        .unwrap_or_else(|| panic!("the live mixed round counts operations: {t:?}"));
    assert!(
        head.contains('▸'),
        "a live row wears the running mark: {head:?}"
    );
    assert!(
        !head.contains("完成") && !head.contains("全部成功") && !head.contains("个失败"),
        "a live round states no outcome yet: {head:?}"
    );
}

/// F — a round with a failure states the failure; a round with a cancelled
/// call never claims success in its place. `44e3be4` must not regress.
#[test]
fn a_round_head_states_failures_and_never_claims_a_false_success() {
    let mut s = opened();
    user(&mut s, "跑两条命令");
    next_assistant(&mut s, "a1");

    // Round 1: one ok, one failed.
    start(&mut s, "ok1", "cargo", r#""check""#, 1);
    finish(&mut s, "ok1", true);
    start(&mut s, "bad1", "cargo", r#""test""#, 1);
    finish(&mut s, "bad1", false);
    next_assistant(&mut s, "a2");

    // Round 2: one ok, one cancelled.
    start(&mut s, "ok2", "git", r#""status""#, 2);
    finish(&mut s, "ok2", true);
    start(&mut s, "stop2", "sleep", r#""120""#, 2);
    cancel(&mut s, "stop2");
    next_assistant(&mut s, "a3");

    let t = lines(&s);
    assert_eq!(recorded_rounds(&s), vec![(Some(1), 2), (Some(2), 2)]);
    let first = t
        .iter()
        .find(|l| l.contains("个失败"))
        .expect("failed process receipt");
    assert!(
        first.contains("个失败"),
        "the failed round says so: {first:?}"
    );
    assert!(
        !first.contains("全部成功"),
        "a round with a failure is never all-ok: {first:?}"
    );

    let second = t
        .iter()
        .find(|l| l.contains("⊘") || l.contains("4 个命令"))
        .expect("cancelled outcome or combined process");
    assert!(
        !second.contains("全部成功"),
        "a cancelled call is not a success: {second:?}"
    );
}

/// G — a round head is structural: it exists from the first call and closing
/// it only rewrites text in place. Asserted on the conversation line list (the
/// screen is bottom-anchored, so new content below legitimately scrolls it).
#[test]
fn a_round_keeps_its_geometry_from_first_call_to_close() {
    let mut s = opened();
    next_assistant(&mut s, "a1");

    start(&mut s, "c1", "cargo", r#""check""#, 1);
    let running = lines(&s);
    let h_run = running
        .iter()
        .position(|l| l.contains("正在执行"))
        .expect("the round head exists from the first call");
    assert!(
        !running.iter().any(|line| line.contains("cargo check")),
        "live member remains folded: {running:?}"
    );

    finish(&mut s, "c1", true);
    start(&mut s, "c2", "cargo", r#""test""#, 1);
    let running_two = lines(&s);
    let h_run2 = running_two
        .iter()
        .position(|l| l.contains("正在执行"))
        .expect("the head is still there with a second call");
    assert_eq!(h_run, h_run2, "adding a call does not move the head");
    assert!(
        !running_two
            .iter()
            .any(|line| line.contains("cargo check") || line.contains("cargo test")),
        "both live members remain folded: {running_two:?}"
    );

    finish(&mut s, "c2", true);
    next_assistant(&mut s, "a2");
    let closed = lines(&s);
    let h_closed = closed
        .iter()
        .position(|l| l.contains("▸") && l.contains("2 个命令"))
        .expect("the head states the outcome once closed");
    assert_eq!(
        h_run2, h_closed,
        "closing rewrote the head in place instead of inserting a row"
    );
    assert!(
        !closed
            .iter()
            .any(|l| l.contains("cargo check") || l.contains("cargo test")),
        "settled children fold into the same parent position: {closed:?}"
    );
    open_latest(&mut s);
    assert!(lines(&s).iter().any(|l| l.contains("cargo check")));
    assert!(lines(&s).iter().any(|l| l.contains("cargo test")));
}

/// Runtime rounds remain independent facts while prose-free work shares a
/// single visible receipt with an accumulated command count.
#[test]
fn a_prose_free_dogfood_run_has_one_receipt_and_retains_all_runtime_rounds() {
    let mut s = opened();
    user(&mut s, "是不是有一个工具在并行改这个项目？");
    next_assistant(&mut s, "a0");

    let rounds: &[(u32, &[(&str, &str)])] = &[
        (
            1,
            &[
                ("ps", r#""aux""#),
                ("pgrep", r#""-fl""#),
                ("lsof", r#""-i""#),
                ("tail", r#""-n""#),
            ],
        ),
        (
            2,
            &[
                ("git", r#""status""#),
                ("git", r#""log""#),
                ("git", r#""rev-parse""#),
            ],
        ),
        (
            3,
            &[
                ("cargo", r#""check""#),
                ("cargo", r#""test""#),
                ("cargo", r#""clippy""#),
            ],
        ),
    ];
    for (round, calls) in rounds {
        for (i, (program, args)) in calls.iter().enumerate() {
            let id = format!("r{round}-{i}");
            start(&mut s, &id, program, args, *round);
            finish(&mut s, &id, true);
        }
    }

    let t = lines(&s);
    assert_eq!(
        recorded_rounds(&s),
        vec![(Some(1), 4), (Some(2), 3), (Some(3), 3)]
    );
    let text = t.join("\n");
    assert!(
        text.contains("执行了 10 个命令"),
        "prose-free rounds accumulate in one phase receipt: {text}"
    );
    next_assistant(&mut s, "done");
    let t = lines(&s);
    assert!(
        t.iter()
            .filter(|l| l.contains("执行了") && l.contains("个命令"))
            .count()
            == 1,
        "settled rounds share one presentation receipt: {t:?}"
    );
    assert!(t.iter().any(|l| l.contains("执行了 10 个命令")));
    assert!(!t.iter().any(|l| l.contains("$ ")));
    open_latest(&mut s);
    assert_eq!(
        lines(&s).iter().filter(|l| l.contains("$ ")).count(),
        20,
        "the keyboard reveals every command header and drawer in the latest phase"
    );
    assert!(
        lines(&s).iter().any(|line| line.contains("$ ps"))
            && lines(&s).iter().any(|line| line.contains("$ git"))
    );
}

/// H — the round identity travels on the event, so a consumer that replays the
/// recorded events groups exactly as the live session did. Dropping the
/// identity (a legacy transcript) falls back to the old shape instead of
/// inventing a boundary.
#[test]
fn replaying_the_recorded_rounds_groups_identically() {
    let events: &[(u32, &str, &str, &str)] = &[
        (1, "a1", "cargo", r#""check""#),
        (1, "a2", "cargo", r#""test""#),
        (2, "b1", "git", r#""status""#),
        (3, "c1", "git", r#""log""#),
        (3, "c2", "git", r#""rev-parse""#),
    ];

    // Live: every call starts and completes, as the runtime forwards it.
    let mut live = opened();
    next_assistant(&mut live, "m0");
    for (round, id, program, args) in events {
        start(&mut live, id, program, args, *round);
        finish(&mut live, id, true);
    }

    // Replay: the same recorded starts, replayed into a fresh client.
    let mut replay = opened();
    next_assistant(&mut replay, "m0");
    for (round, id, program, args) in events {
        start(&mut replay, id, program, args, *round);
    }

    assert_eq!(
        recorded_rounds(&live),
        vec![(Some(1), 2), (Some(2), 1), (Some(3), 2)]
    );
    assert_eq!(
        recorded_rounds(&replay),
        recorded_rounds(&live),
        "round membership is independent of the collapsed projection"
    );

    // A legacy transcript with no identity keeps the pre-round shape: the
    // round head does not exist, so the old activity-class grouping stands in.
    let mut legacy = opened();
    next_assistant(&mut legacy, "m0");
    for (_, id, program, args) in events {
        reduce(
            &mut legacy,
            Action::Runtime(RuntimeEvent::ToolCallStarted {
                id: ToolCallId::new(*id),
                name: "run_command".into(),
                arguments: format!(r#"{{"program":"{program}","args":[{args}]}}"#),
                parallel: false,
                model_step: None,
                answer_effect: None,
            }),
        );
        finish(&mut legacy, id, true);
    }
    let legacy_lines = lines(&legacy);
    assert!(
        round_head_rows(&legacy_lines).is_empty(),
        "without an identity the round head does not exist: {legacy_lines:?}"
    );
}
