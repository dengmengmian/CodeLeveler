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

/// A — a homogeneous shell round keeps the tool's own name (`执行命令`), and
/// never falls back to an abstract count. The same rule covers every
/// same-tool round: reads name reads, searches name searches.
#[test]
fn a_homogeneous_shell_round_names_the_tool() {
    let mut s = opened();
    next_assistant(&mut s, "a1");
    start(&mut s, "c1", "cargo", r#""check""#, 1);
    finish(&mut s, "c1", true);
    start(&mut s, "c2", "cargo", r#""test""#, 1);
    finish(&mut s, "c2", true);

    let t = lines(&s);
    let head = t
        .iter()
        .find(|l| l.contains("执行命令"))
        .unwrap_or_else(|| panic!("the shell round names the tool: {t:?}"));
    assert!(head.contains("完成 2 项"), "it counts both calls: {head:?}");
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
        .find(|l| l.contains("完成 2 项操作"))
        .unwrap_or_else(|| panic!("the mixed round counts its operations: {t:?}"));
    assert!(
        !head.contains("多个工具"),
        "the abstract 'multiple tools' label is gone: {head:?}"
    );
    assert!(
        !head.contains("执行了"),
        "the mixed round is not a stage sentence: {head:?}"
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
        .find(|l| l.contains("完成 3 项操作"))
        .unwrap_or_else(|| panic!("the mixed round counts every operation: {t:?}"));
    assert!(
        head.contains("1 个失败"),
        "the failure still rides the head: {head:?}"
    );
    assert!(
        !head.contains("多个工具"),
        "the abstract label is gone: {head:?}"
    );
    assert!(
        t.iter().any(|l| l.contains("src/lib.rs")),
        "the failed call keeps its own row: {t:?}"
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
        .find(|l| l.contains("正在执行 2 项操作"))
        .unwrap_or_else(|| panic!("the live mixed round counts operations: {t:?}"));
    assert!(
        head.contains('\u{22ee}'),
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
    let heads = round_head_rows(&t);
    assert_eq!(heads.len(), 2, "two rounds, two heads: {t:?}");

    let first = &t[heads[0]];
    assert!(
        first.contains("个失败"),
        "the failed round says so: {first:?}"
    );
    assert!(
        !first.contains("全部成功"),
        "a round with a failure is never all-ok: {first:?}"
    );

    let second = &t[heads[1]];
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
        running[h_run + 1].contains("\u{2514}\u{2500}"),
        "the sole child follows the head while running: {running:?}"
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
        running_two[h_run2 + 1].contains("\u{251c}\u{2500}"),
        "the first child follows the head while running: {running_two:?}"
    );

    finish(&mut s, "c2", true);
    next_assistant(&mut s, "a2");
    let closed = lines(&s);
    let h_closed = closed
        .iter()
        .position(|l| l.contains("完成"))
        .expect("the head states the outcome once closed");
    assert_eq!(
        h_run2, h_closed,
        "closing rewrote the head in place instead of inserting a row"
    );
    assert!(
        closed[h_closed + 1].contains("\u{251c}\u{2500}"),
        "the first child did not jump: {closed:?}"
    );

    // The column the children start at does not change across the close.
    let column = |line: &str| line.chars().take_while(|c| *c == ' ').count();
    assert_eq!(
        column(&running_two[h_run2 + 1]),
        column(&closed[h_closed + 1]),
        "the children keep their column across the close"
    );
}

/// The reported Dogfood shape: many prose-free model rounds, each a handful of
/// shell commands. They must be separate rounds, not one 25+ call group.
#[test]
fn a_prose_free_dogfood_run_is_one_group_per_round() {
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
    let heads = round_head_rows(&t);
    assert_eq!(
        heads.len(),
        rounds.len(),
        "each prose-free model response is its own round: {t:?}"
    );
    let text = t.join("\n");
    assert!(
        !text.contains("执行了 10 个命令"),
        "the rounds must not collapse into one group: {text}"
    );
    // Every command keeps its own row.
    assert_eq!(
        t.iter().filter(|l| l.contains("$ ")).count(),
        4 + 3 + 3,
        "every call keeps a row: {t:?}"
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

    let live_heads = round_head_rows(&lines(&live));
    let replay_heads = round_head_rows(&lines(&replay));
    assert_eq!(live_heads.len(), 3, "live: three rounds");
    assert_eq!(
        replay_heads.len(),
        live_heads.len(),
        "a replay regroups exactly as the live session did"
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
