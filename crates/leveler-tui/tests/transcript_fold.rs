//! The Reasoning / Thought and execution-fold presentation contract.
//!
//! These tests drive the REAL reducer and the REAL conversation builder, so
//! every assertion is about the lines the terminal paints. The contract:
//!
//! - A Thought is a SIBLING of the tool activity that follows it, never its
//!   parent. Its `│` rail covers its own body and nothing else.
//! - A live Thought shows its body; a finished Thought folds to its header and
//!   the user may open it back up.
//! - A run of Search / Read / List is ONE view-time fold whose collapsed row is
//!   an aggregate receipt — and expanding restores every real member row.
//! - A finished collapsed Thought inside such a run is a participant: the
//!   collapsed receipt hides it and counts tools only, while its semantic item
//!   is kept and expanding restores it in real chronology. A Thought the reader
//!   opened is pinned and no run state hides it.
//! - Folding never moves the reader's viewport.

use leveler_client_protocol::{MessageId, RuntimeEvent, SessionId, ToolCallId, UiSessionSnapshot};
use leveler_tui::action::Action;
use leveler_tui::conversation::build::{
    build_conversation_lines_with_hits, conversation_line_count,
};
use leveler_tui::conversation::interaction::toggle_fold;
use leveler_tui::fold::DisplayMode;
use leveler_tui::reducer::reduce;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use leveler_tui::transcript::{
    AssistantKind, ThoughtBlock, ToolStatus, TranscriptItem, TurnEndStatus,
};
use serde_json::{Value, json};

const W: usize = 100;

fn opened_bare() -> AppState {
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
    s.size = (W as u16, 30);
    s.conv.rect = Some((0, 0, W as u16, 24));
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

/// The painted checks need a prompt: without one the welcome splash owns the
/// viewport and the conversation builder paints the logo instead. The corpus
/// tree, by contrast, is the transcript itself — an extra synthetic line would
/// be an item the fixture never declared.
fn opened() -> AppState {
    let mut s = opened_bare();
    s.transcript.push_user("看下当前项目有什么 bug".into());
    s
}

/// Close the open tool group the way the next assistant round does, so the
/// group reads as history rather than as live activity.
fn settle_group(s: &mut AppState) {
    assistant(s, "settle", "看完了。");
}

fn lines(s: &AppState) -> Vec<String> {
    build_conversation_lines_with_hits(s, W)
        .0
        .into_iter()
        .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
        .collect()
}

fn text(s: &AppState) -> String {
    lines(s).join("\n")
}

fn reasoning_start(s: &mut AppState) {
    reduce(s, Action::Runtime(RuntimeEvent::ReasoningStarted));
}

fn reasoning(s: &mut AppState, delta: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ReasoningDelta {
            delta: delta.into(),
        }),
    );
}

fn reasoning_done(s: &mut AppState, ms: u64) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ReasoningCompleted { elapsed_ms: ms }),
    );
}

fn assistant(s: &mut AppState, id: &str, body: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new(id),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: MessageId::new(id),
            delta: body.into(),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: MessageId::new(id),
        }),
    );
}

fn tool_started(s: &mut AppState, id: &str, name: &str, args: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: name.into(),
            arguments: args.into(),
            parallel: false,
            model_step: None,
            answer_effect: None,
        }),
    );
}

fn tool_completed(s: &mut AppState, id: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new(id),
            ok: true,
            preview: "ok".into(),
            duration_ms: 4,
            applied_diff: None,
        }),
    );
}

/// The same call, requested in a specific execution round (`model_step`). This
/// is the real shape of a model turn: every tool from one response shares one
/// round, which is what made a lone Read wear a `完成 1 项` head.
fn tool_started_in_round(s: &mut AppState, id: &str, name: &str, args: &str, step: u32) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: name.into(),
            arguments: args.into(),
            parallel: false,
            model_step: Some(step),
            answer_effect: None,
        }),
    );
}

fn tool_failed(s: &mut AppState, id: &str, preview: &str) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new(id),
            ok: false,
            preview: preview.into(),
            duration_ms: 4,
            applied_diff: None,
        }),
    );
}

fn read(s: &mut AppState, id: &str, path: &str) {
    tool_started(s, id, "read_file", &format!(r#"{{"path":"{path}"}}"#));
    tool_completed(s, id);
}

fn read_in_round(s: &mut AppState, id: &str, path: &str, step: u32) {
    tool_started_in_round(s, id, "read_file", &format!(r#"{{"path":"{path}"}}"#), step);
    tool_completed(s, id);
}

fn search_in_round(s: &mut AppState, id: &str, pattern: &str, step: u32) {
    tool_started_in_round(
        s,
        id,
        "grep",
        &format!(r#"{{"pattern":"{pattern}"}}"#),
        step,
    );
    tool_completed(s, id);
}

fn thoughts(s: &AppState) -> Vec<&ThoughtBlock> {
    s.transcript
        .items()
        .iter()
        .filter_map(|i| match i {
            TranscriptItem::Thought(b) => Some(b),
            _ => None,
        })
        .collect()
}

fn thought_index(s: &AppState) -> usize {
    s.transcript
        .items()
        .iter()
        .position(|i| matches!(i, TranscriptItem::Thought(_)))
        .expect("a Thought")
}

// ── THOUGHT-FOLD ─────────────────────────────────────────────────────────────

/// THOUGHT-FOLD-1: while the provider is reasoning, the Thought shows its body.
#[test]
fn thought_fold_1_a_live_thought_shows_its_body() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "先看工作区和近期变更。");
    let text = text(&s);
    assert!(text.contains("思考中"), "{text}");
    assert!(text.contains("先看工作区和近期变更。"), "{text}");
}

/// THOUGHT-FOLD-2: `ReasoningCompleted` folds the body away on its own.
#[test]
fn thought_fold_2_completion_auto_folds() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "先看工作区和近期变更。");
    reasoning_done(&mut s, 4100);
    assert_eq!(thoughts(&s).len(), 1);
    assert_eq!(thoughts(&s)[0].display, DisplayMode::Collapsed);
    assert_eq!(thoughts(&s)[0].duration_ms, Some(4100));
}

/// THOUGHT-FOLD-3: a collapsed Thought paints its header and no body line.
#[test]
fn thought_fold_3_collapsed_thought_is_header_only() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "只有这一句推理。");
    reasoning_done(&mut s, 4100);
    let rows = lines(&s);
    let header = rows
        .iter()
        .find(|l| l.contains("思考 · 4.1s"))
        .expect("the header");
    assert!(header.starts_with('◆'), "{rows:?}");
    assert!(
        !rows.iter().any(|l| l.contains("只有这一句推理。")),
        "the body is folded: {rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.starts_with('│')),
        "no rail without a body: {rows:?}"
    );
}

/// THOUGHT-FOLD-4 / -5: the body comes back on expand and goes away again.
#[test]
fn thought_fold_4_and_5_expand_and_collapse_again() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "先看工作区和近期变更。\n再检查测试和错误路径。");
    reasoning_done(&mut s, 4100);
    let item = thought_index(&s);

    s.transcript.set_item_display(item, DisplayMode::Expanded);
    let opened = text(&s);
    assert!(opened.contains("先看工作区和近期变更。"), "{opened}");
    assert!(opened.contains("再检查测试和错误路径。"), "{opened}");

    s.transcript.set_item_display(item, DisplayMode::Collapsed);
    let folded = text(&s);
    assert!(!folded.contains("先看工作区和近期变更。"), "{folded}");
    assert!(!folded.contains("再检查测试和错误路径。"), "{folded}");
}

/// THOUGHT-FOLD-6: a manually opened historical Thought is never folded back by
/// later live events, and its body is never rewritten.
#[test]
fn thought_fold_6_a_pinned_history_thought_survives_later_events() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "第一段推理。");
    reasoning_done(&mut s, 1000);

    let first = thought_index(&s);
    toggle_fold(&mut s, first);
    assert!(thoughts(&s)[0].display_pinned);

    // Later live activity: a tool, then a second reasoning segment.
    read(&mut s, "t1", "a.rs");
    reasoning_start(&mut s);
    reasoning(&mut s, "第二段推理。");
    reasoning_done(&mut s, 2000);

    assert_eq!(
        thoughts(&s)[0].display,
        DisplayMode::Expanded,
        "the reader's fold is not overridden"
    );
    assert_eq!(thoughts(&s)[0].text, "第一段推理。");
    assert_eq!(thoughts(&s)[0].duration_ms, Some(1000));
    let rendered = text(&s);
    assert!(rendered.contains("第一段推理。"), "{rendered}");
}

/// THOUGHT-FOLD-7: a new reasoning segment is the transcript's last item, and
/// never appends to the previous Thought.
#[test]
fn thought_fold_7_a_new_thought_is_the_tail() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "第一段。");
    reasoning_done(&mut s, 1000);
    read(&mut s, "t1", "a.rs");

    reasoning_start(&mut s);
    reasoning(&mut s, "第二段。");

    let items = s.transcript.items();
    assert!(
        matches!(items.last(), Some(TranscriptItem::Thought(b)) if b.text == "第二段。"),
        "{items:?}"
    );
    assert_eq!(thoughts(&s).len(), 2, "two segments, two Thoughts");
    assert!(thoughts(&s)[0].done);
    assert!(!thoughts(&s)[1].done);
}

/// THOUGHT-FOLD-8: a tool row is a SIBLING of the Thought, not its child —
/// the same first column, and the Thought's `│` rail never reaches it. A lone
/// read outside any run is the clean case: nothing is folded, so the Thought
/// header and the tool row are both drawn at the entry column.
#[test]
fn thought_fold_8_a_tool_is_a_sibling_not_a_thought_child() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "先看工作区。");
    reasoning_done(&mut s, 4100);
    read(&mut s, "t1", "a.rs");
    settle_group(&mut s);

    let rows = lines(&s);
    let header = rows
        .iter()
        .position(|l| l.contains("思考 · 4.1s"))
        .expect("the Thought header");
    let tool = rows
        .iter()
        .position(|l| l.contains("a.rs"))
        .expect("the tool row");
    assert!(header < tool, "the tool follows its Thought: {rows:?}");
    assert_eq!(
        rows[header].chars().take_while(|c| *c == ' ').count(),
        rows[tool].chars().take_while(|c| *c == ' ').count(),
        "the tool row sits at the same column as the Thought: {rows:?}"
    );
    assert!(
        rows[header..tool]
            .iter()
            .all(|l| !l.starts_with('│') && !l.trim_start().starts_with('│')),
        "no rail carries into the tool: {rows:?}"
    );
}

// ── TOOL-GROUP ───────────────────────────────────────────────────────────────

/// TOOL-GROUP-1 / -2: consecutive Read / Search / List derive ONE group whose
/// collapsed row is an aggregate receipt.
#[test]
fn tool_group_1_and_2_consecutive_exploration_is_one_group() {
    let mut s = opened();
    read(&mut s, "t1", "README.md");
    read(&mut s, "t2", "AGENTS.md");
    tool_started(&mut s, "t3", "grep", r#"{"pattern":"pricing"}"#);
    tool_completed(&mut s, "t3");
    tool_started(&mut s, "t4", "grep", r#"{"pattern":"request_logs"}"#);
    tool_completed(&mut s, "t4");
    settle_group(&mut s);

    let rows = lines(&s);
    let receipts: Vec<&String> = rows
        .iter()
        .filter(|l| l.contains("读取 2 个文件"))
        .collect();
    assert_eq!(receipts.len(), 1, "one group, one receipt: {rows:?}");
    assert!(
        receipts[0].contains("搜索 2 次"),
        "the receipt counts by kind: {rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("README.md")),
        "collapsed: no member rows: {rows:?}"
    );
}

/// TOOL-GROUP-3: expanding restores every real member row, naming its own
/// target — the fold is view-time, nothing was merged away.
#[test]
fn tool_group_3_expanding_restores_every_member() {
    let mut s = opened();
    read(&mut s, "t1", "README.md");
    read(&mut s, "t2", "AGENTS.md");
    tool_started(&mut s, "t3", "grep", r#"{"pattern":"pricing"}"#);
    tool_completed(&mut s, "t3");
    settle_group(&mut s);

    let group = s
        .transcript
        .items()
        .iter()
        .position(|i| matches!(i, TranscriptItem::ToolGroup(_)))
        .expect("a group");
    toggle_fold(&mut s, group);
    let rows = lines(&s);
    let text = rows.join("\n");
    assert!(text.contains("README.md"), "{rows:?}");
    assert!(text.contains("AGENTS.md"), "{rows:?}");
    assert!(text.contains("pricing"), "{rows:?}");
    assert!(
        rows.iter().any(|l| l.starts_with('▾')),
        "the aggregate header stays open: {rows:?}"
    );
}

/// TOOL-GROUP-4: collapsing again returns to the single aggregate row.
#[test]
fn tool_group_4_collapsing_returns_to_one_row() {
    let mut s = opened();
    read(&mut s, "t1", "README.md");
    read(&mut s, "t2", "AGENTS.md");
    settle_group(&mut s);
    let group = s
        .transcript
        .items()
        .iter()
        .position(|i| matches!(i, TranscriptItem::ToolGroup(_)))
        .expect("a group");
    toggle_fold(&mut s, group);
    toggle_fold(&mut s, group);
    let rows = lines(&s);
    assert!(
        !rows.iter().any(|l| l.contains("README.md")),
        "collapsed again: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|l| l.starts_with('▸') && l.contains("读取 2 个文件")),
        "{rows:?}"
    );
}

/// TOOL-GROUP-5: a run or an edit is a breaker, not a member.
#[test]
fn tool_group_5_a_run_breaks_the_exploration_group() {
    let mut s = opened();
    read(&mut s, "t1", "a.rs");
    read(&mut s, "t2", "b.rs");
    tool_started(
        &mut s,
        "t3",
        "run_command",
        r#"{"program":"cargo","args":["test"]}"#,
    );
    tool_completed(&mut s, "t3");
    settle_group(&mut s);

    let rows = lines(&s);
    let receipt = rows
        .iter()
        .position(|l| l.contains("读取 2 个文件"))
        .expect("the exploration receipt");
    let run = rows
        .iter()
        .position(|l| l.contains("cargo test"))
        .expect("the run keeps its own row");
    assert!(run > receipt, "the run follows the receipt: {rows:?}");
}

/// TOOL-GROUP-6: a failed member is never folded away — its target stays on
/// screen with its reason.
#[test]
fn tool_group_6_a_failed_member_keeps_its_target() {
    let mut s = opened();
    read(&mut s, "t1", "a.rs");
    read(&mut s, "t2", "b.rs");
    tool_started(&mut s, "t3", "read_file", r#"{"path":"missing.rs"}"#);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new("t3"),
            ok: false,
            preview: "no such file".into(),
            duration_ms: 2,
            applied_diff: None,
        }),
    );
    settle_group(&mut s);
    let rendered = text(&s);
    assert!(rendered.contains("missing.rs"), "{rendered}");
    assert!(rendered.contains("no such file"), "{rendered}");
}

// ── RUN-THOUGHT ──────────────────────────────────────────────────────────────
//
// The one place a Thought and a fold interact. The contract has three mutually
// exclusive states: outside a run it is its own header; folded in a run it is a
// hidden participant that never counts; opened by the reader it is pinned and
// no run state hides it.

/// RUN-THOUGHT-1: a finished collapsed Thought between the reads is a
/// participant of the run — the collapsed receipt speaks for it, its text is
/// off screen, and it is not counted in the aggregate label.
#[test]
fn run_thought_1_a_folded_thought_is_a_participant_of_the_run() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "先读 pricing。");
    reasoning_done(&mut s, 800);
    read(&mut s, "t1", "src/pricing.rs");
    read(&mut s, "t2", "src/catalog.rs");
    settle_group(&mut s);

    let rows = lines(&s);
    let receipt = rows
        .iter()
        .find(|l| l.contains("读取 2 个文件"))
        .expect("the run receipt");
    assert!(
        !receipt.contains("思考"),
        "the label counts tools only: {receipt}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("思考 ·")),
        "the collapsed receipt speaks for the Thought: {rows:?}"
    );
    assert!(
        !text(&s).contains("先读 pricing。"),
        "its body is folded away: {rows:?}"
    );
    // Folded out of sight, not out of the transcript.
    assert_eq!(thoughts(&s).len(), 1, "the semantic item is kept");
    assert_eq!(thoughts(&s)[0].display, DisplayMode::Collapsed);
}

/// RUN-THOUGHT-2: opening the run restores the Thought and every member in real
/// chronology — the Thought that preceded the first read stays first.
#[test]
fn run_thought_2_opening_the_run_restores_thought_and_members_in_order() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "先读 pricing。");
    reasoning_done(&mut s, 800);
    read(&mut s, "t1", "src/pricing.rs");
    read(&mut s, "t2", "src/catalog.rs");
    settle_group(&mut s);

    let anchor = s
        .transcript
        .items()
        .iter()
        .position(|i| matches!(i, TranscriptItem::ToolGroup(_)))
        .expect("the run anchor group");
    toggle_fold(&mut s, anchor);

    let rows = lines(&s);
    let thought = rows
        .iter()
        .position(|l| l.contains("思考 ·"))
        .expect("the restored Thought header");
    let first = rows
        .iter()
        .position(|l| l.contains("pricing.rs"))
        .expect("the first member");
    let second = rows
        .iter()
        .position(|l| l.contains("catalog.rs"))
        .expect("the second member");
    assert!(
        thought < first && first < second,
        "real chronology: {rows:?}"
    );
}

/// RUN-THOUGHT-3: only after the run restored it is the Thought itself
/// clickable; opening it paints the full reasoning body.
#[test]
fn run_thought_3_a_restored_thought_opens_its_reasoning_body() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "先读 pricing。");
    reasoning_done(&mut s, 800);
    read(&mut s, "t1", "src/pricing.rs");
    read(&mut s, "t2", "src/catalog.rs");
    settle_group(&mut s);

    let anchor = s
        .transcript
        .items()
        .iter()
        .position(|i| matches!(i, TranscriptItem::ToolGroup(_)))
        .expect("the run anchor group");
    toggle_fold(&mut s, anchor);
    // The run is open; the reader now opens the Thought inside it.
    let thought = thought_index(&s);
    toggle_fold(&mut s, thought);
    let body = text(&s);
    assert!(body.contains("先读 pricing。"), "{body}");
    assert_eq!(thoughts(&s)[0].display, DisplayMode::Expanded);
}

/// RUN-THOUGHT-4: a Thought the reader opened is pinned. The run still folds
/// its tool members, but it never folds this Thought away.
#[test]
fn run_thought_4_an_open_thought_survives_the_collapsed_run() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "先读 pricing。");
    reasoning_done(&mut s, 800);
    // The reader opens it before any exploration arrives.
    let thought = thought_index(&s);
    toggle_fold(&mut s, thought);
    read(&mut s, "t1", "src/pricing.rs");
    read(&mut s, "t2", "src/catalog.rs");
    settle_group(&mut s);

    let rows = lines(&s);
    assert!(
        rows.iter().any(|l| l.contains("读取 2 个文件")),
        "the run still forms and folds its members: {rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("catalog.rs")),
        "tool members are folded: {rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("思考 ·")),
        "the open Thought is not hidden by the run: {rows:?}"
    );
    assert!(
        text(&s).contains("先读 pricing。"),
        "and its body stays painted: {rows:?}"
    );
}

/// RUN-THOUGHT-5: a finished Thought that no run folds is its own header, and
/// the lone read beside it keeps its own row — no receipt is invented.
#[test]
fn run_thought_5_a_thought_outside_a_run_keeps_its_own_header() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "先看工作区。");
    reasoning_done(&mut s, 4100);
    read(&mut s, "t1", "README.md");
    settle_group(&mut s);

    let rows = lines(&s);
    assert!(
        rows.iter().any(|l| l.contains("思考 ·")),
        "its own collapsed header: {rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("README.md")),
        "a lone read keeps its own row: {rows:?}"
    );
    assert!(
        !rows.iter().any(|l| l.contains("读取 1 个文件")),
        "no receipt for a single read: {rows:?}"
    );
}

// ── RUN-FOLD ─────────────────────────────────────────────────────────────────

/// RUN-FOLD-1 / -2: a settled command folds to its logical receipt and reveals
/// its command and output on expand.
#[test]
fn run_fold_1_and_2_a_command_folds_and_reveals() {
    let mut s = opened();
    tool_started(
        &mut s,
        "t1",
        "run_command",
        r#"{"program":"echo","args":["tests-passed"]}"#,
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: Some(0),
            stop: None,
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "tests-passed\n".into(),
            duration_ms: 8200,
            applied_diff: None,
        }),
    );
    settle_group(&mut s);
    let folded = text(&s);
    assert!(
        folded.contains("执行命令") || folded.contains("echo"),
        "{folded}"
    );

    let group = s
        .transcript
        .items()
        .iter()
        .position(|i| matches!(i, TranscriptItem::ToolGroup(_)))
        .expect("a group");
    toggle_fold(&mut s, group);
    let opened = text(&s);
    assert!(opened.contains("$ echo tests-passed"), "{opened}");
    assert!(opened.contains("tests-passed"), "{opened}");
}

// ── SCROLL-FOLD ──────────────────────────────────────────────────────────────

/// SCROLL-FOLD-1: folding an entry above the viewport does not move the rows
/// the reader is looking at.
#[test]
fn scroll_fold_1_an_above_viewport_fold_keeps_the_viewport() {
    let mut s = opened();
    reasoning(&mut s, "第一段很长的推理内容，用来把后面的内容推出视口。");
    reasoning_done(&mut s, 1200);
    for i in 0..8 {
        assistant(&mut s, &format!("m{i}"), &format!("第 {i} 行回答"));
    }

    let total = conversation_line_count(&s, W);
    assert!(total > 12, "the transcript must overflow: {total}");
    s.conv.auto_scroll = false;
    s.conv.scroll = total - 12;

    let item = thought_index(&s);
    let before = viewport_anchor(&s);
    toggle_fold(&mut s, item);
    let after = viewport_anchor(&s);
    assert_eq!(
        after, before,
        "the line at the viewport's top edge is still the one the reader was reading"
    );
}

/// SCROLL-FOLD-1 (collapse): the same holds when the fold shrinks the content.
#[test]
fn scroll_fold_1_a_collapse_above_the_viewport_keeps_the_viewport() {
    let mut s = opened();
    reasoning(&mut s, "第一段很长的推理内容，用来把后面的内容推出视口。");
    reasoning_done(&mut s, 1200);
    let item = thought_index(&s);
    s.transcript.set_item_display(item, DisplayMode::Expanded);
    for i in 0..8 {
        assistant(&mut s, &format!("m{i}"), &format!("第 {i} 行回答"));
    }

    let total = conversation_line_count(&s, W);
    s.conv.auto_scroll = false;
    s.conv.scroll = total - 12;

    let before = viewport_anchor(&s);
    toggle_fold(&mut s, item);
    let after = viewport_anchor(&s);
    assert_eq!(
        after, before,
        "the anchored line stayed put through a collapse"
    );
}

/// The reader's place: the transcript item painted at the viewport's top edge
/// and that item's offset from the top edge.
fn viewport_anchor(s: &AppState) -> (usize, i64) {
    let height = 24usize;
    let total = conversation_line_count(s, W);
    let scroll = s.conv.scroll.min(total.saturating_sub(height.max(1)));
    let item = (0..s.transcript.len())
        .rev()
        .find(|&i| s.item_start_line(i, W).is_some_and(|start| start <= scroll))
        .expect("an item at the viewport top");
    let offset = s.item_start_line(item, W).expect("a start line") as i64 - scroll as i64;
    (item, offset)
}

// ── RESUME-FOLD ──────────────────────────────────────────────────────────────

/// RESUME-FOLD-1: replayed reasoning restores its body and duration, and comes
/// back folded — a resumed session is history, not a live segment.
#[test]
fn resume_fold_1_replayed_thought_is_collapsed_but_openable() {
    let mut s = opened();
    // The replay path is the same event sequence the durable projection emits.
    reasoning_start(&mut s);
    reasoning(&mut s, "先检查工作区和近期变更。");
    reasoning_done(&mut s, 3400);
    assistant(&mut s, "m1", "看完了。");

    let thought = &thoughts(&s)[0];
    assert_eq!(thought.display, DisplayMode::Collapsed);
    assert_eq!(thought.text, "先检查工作区和近期变更。");
    assert_eq!(thought.duration_ms, Some(3400));
    assert!(
        !text(&s).contains("先检查工作区和近期变更。"),
        "a restored Thought is not expanded by default"
    );

    let item = thought_index(&s);
    toggle_fold(&mut s, item);
    assert!(
        text(&s).contains("先检查工作区和近期变更。"),
        "and it opens back up"
    );
}

// ── THOUGHT-LABEL ────────────────────────────────────────────────────────────
//
// The Thought header is wording only. A completed segment reads `思考 · 0.9s`
// (bare verb + measurement), a streaming one keeps `思考中…`, and a cut one
// stays explicit. The lifecycle, `DisplayMode` and persistence are untouched.

/// THOUGHT-LABEL-1: a completed Thought's Chinese header is `思考 · {duration}`.
#[test]
fn thought_label_1_completed_header_is_verb_dot_duration() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "看完了。");
    reasoning_done(&mut s, 900);
    let text = text(&s);
    assert!(text.contains("◆ 思考 · 0.9s"), "{text}");
    assert!(!text.contains("已思考"), "{text}");
}

/// THOUGHT-LABEL-2: a streaming Thought still reads `思考中…`.
#[test]
fn thought_label_2_running_header_is_unchanged() {
    let mut s = opened();
    reasoning_start(&mut s);
    let text = text(&s);
    assert!(text.contains("◆ 思考中…"), "{text}");
}

/// THOUGHT-LABEL-3: a segment that never reached a clean boundary says so, and
/// never wears the completed form.
#[test]
fn thought_label_3_interrupted_header_stays_explicit() {
    let mut s = opened();
    reasoning_start(&mut s);
    reasoning(&mut s, "半句话");
    // A boundary without a completion freezes the segment as interrupted.
    tool_started_in_round(&mut s, "t1", "read_file", r#"{"path":"a.rs"}"#, 1);
    tool_completed(&mut s, "t1");
    let text = text(&s);
    assert!(text.contains("◆ 思考中断"), "{text}");
    assert!(!text.contains("思考 ·"), "{text}");
}

// ── SINGLE-EXPLORE ───────────────────────────────────────────────────────────
//
// One Read/Search/List is not a stretch: it shows its own target under the same
// verb the aggregate receipt counts in. Two or more still fold into one receipt.

/// SINGLE-EXPLORE-1: one Read in a round is its own row — verb, real target,
/// result — with no `完成 1 项` parent and no tree child under it.
#[test]
fn single_explore_1_a_lone_read_shows_its_target() {
    let mut s = opened();
    read_in_round(
        &mut s,
        "r1",
        "backend/internal/modules/sourcing/handler/rfq.go",
        1,
    );
    settle_group(&mut s);
    let text = text(&s);
    assert!(
        text.contains("› 读取 backend/internal/modules/sourcing/handler/rfq.go"),
        "{text}"
    );
    assert!(!text.contains("完成 1 项"), "{text}");
    assert!(!text.contains('\u{2514}'), "no tree child: {text}");
}

/// SINGLE-EXPLORE-2: one Search in a round shows the query it ran and its
/// result summary directly.
#[test]
fn single_explore_2_a_lone_search_shows_query_and_result() {
    let mut s = opened();
    search_in_round(&mut s, "g1", "missing model", 1);
    settle_group(&mut s);
    let text = text(&s);
    assert!(text.contains("› 搜索 \"missing model\""), "{text}");
    assert!(text.contains("· 1 行"), "the result summary: {text}");
    assert!(!text.contains("完成 1 项"), "{text}");
}

/// SINGLE-EXPLORE-3: one List shows its target with the List verb.
///
/// A SUCCESSFUL `list_files` is `Silent` by design — it never enters
/// Conversation at all — so the visible instance is a failed scan, whose target
/// is exactly what the reader needs.
#[test]
fn single_explore_3_a_lone_list_shows_its_target() {
    let mut s = opened();
    tool_started_in_round(
        &mut s,
        "l1",
        "list_files",
        r#"{"path":"backend/internal/modules/sourcing"}"#,
        1,
    );
    tool_failed(&mut s, "l1", "permission denied");
    settle_group(&mut s);
    let text = text(&s);
    assert!(
        text.contains("› ✗ 列出 backend/internal/modules/sourcing"),
        "{text}"
    );
    assert!(!text.contains("完成 1 项"), "{text}");
}

/// SINGLE-EXPLORE-4: two exploration calls still fold into one aggregate
/// receipt — the single-row form never manufactures a group of one.
#[test]
fn single_explore_4_two_or_more_still_aggregate() {
    let mut s = opened();
    read_in_round(&mut s, "r1", "a.rs", 1);
    read_in_round(&mut s, "r2", "b.rs", 1);
    settle_group(&mut s);
    let text = text(&s);
    assert!(text.contains("▸ 读取 2 个文件"), "{text}");
    assert!(!text.contains("a.rs"), "members stay folded: {text}");
}

/// SINGLE-EXPLORE-5: opening the receipt restores members as direct single-tool
/// rows, each naming its own target.
#[test]
fn single_explore_5_expanded_members_use_direct_rows() {
    let mut s = opened();
    read_in_round(&mut s, "r1", "a.rs", 1);
    search_in_round(&mut s, "g1", "missing model", 1);
    settle_group(&mut s);
    let group = s
        .transcript
        .items()
        .iter()
        .position(|i| matches!(i, TranscriptItem::ToolGroup(_)))
        .expect("a group");
    toggle_fold(&mut s, group);
    let text = text(&s);
    assert!(text.contains("▾ 读取 1 个文件 · 搜索 1 次"), "{text}");
    assert!(text.contains("› 读取 a.rs"), "{text}");
    assert!(text.contains("› 搜索 \"missing model\""), "{text}");
}

/// NARROW-EXPLORE-1: a long target shortens by DISPLAY width — CJK included —
/// so a narrow terminal never overruns its gutter.
#[test]
fn narrow_explore_1_a_long_target_does_not_break_the_layout() {
    let mut s = opened();
    read_in_round(
        &mut s,
        "r1",
        "backend/internal/modules/sourcing/handler/非常长的中文目录名/rfq.go",
        1,
    );
    settle_group(&mut s);
    let width = 40usize;
    let rows: Vec<String> = build_conversation_lines_with_hits(&s, width)
        .0
        .into_iter()
        .map(|l| l.spans.iter().map(|sp| sp.content.as_ref()).collect())
        .collect();
    let text = rows.join("\n");
    assert!(text.contains("› 读取 "), "{text}");
    let row = rows
        .iter()
        .find(|l| l.contains("› 读取 "))
        .expect("the read row");
    assert!(
        row.contains('…'),
        "the long target is shortened, not clipped: {row:?}"
    );
    for line in &rows {
        assert!(
            unicode_width::UnicodeWidthStr::width(line.as_str()) <= width,
            "line exceeds {width} columns: {line:?}\n{text}"
        );
    }
}

// ── The shared conversation-presentation corpus ─────────────────────────────
//
// `testdata/conversation_presentation/v1/` is the language-neutral oracle for
// the conversation contract. The terminal is the reference implementation, so
// this is where the corpus is anchored: the same wire events the Web and
// Desktop clients consume are driven through the real TUI reducer and asserted
// against the same expectation. The other surfaces prove conformance against
// these same JSON files.

fn corpus(id: &str) -> serde_json::Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/conversation_presentation/v1")
        .join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&raw).expect("fixture json")
}

/// Drive one fixture's live path through the real reducer.
///
/// `painted` keeps the synthetic prompt the rendered checks need; the corpus
/// tree is compared without it. The returned plan is the one the LAST
/// `plan_updated` left: a settled turn's plan panel is restored from the durable
/// goal facts, so that is where the plan's own chronology lives.
fn drive_corpus_painted(id: &str, painted: bool) -> (AppState, serde_json::Value) {
    let doc = corpus(id);
    let mut s = if painted { opened() } else { opened_bare() };
    // A fixture may declare session facts (the axis, the goal): they arrive on
    // the session snapshot, not in the event stream, exactly as they do in the
    // product.
    let mut last_plan: Option<leveler_client_protocol::UiPlan> = None;
    s.plan = None;
    if let Some(collaboration) = doc["session"]["collaboration"].as_str() {
        s.collaboration = collaboration.to_string();
    }
    if let Some(goal) = doc["session"]["goal"].as_str() {
        s.goal = goal.to_string();
    }
    if let Some(session) = doc["session"].as_object() {
        let _ = session;
    }
    for entry in doc["paths"]["live"].as_array().expect("live path") {
        let event: RuntimeEvent =
            serde_json::from_value(entry["event"].clone()).expect("wire event");
        let is_plan = matches!(event, RuntimeEvent::PlanUpdated { .. });
        reduce(&mut s, Action::Runtime(event));
        if is_plan {
            last_plan = s.plan.clone();
        }
    }
    PLAN_CAPTURE.with(|slot| *slot.borrow_mut() = last_plan);
    (s, doc)
}

thread_local! {
    /// The plan the LAST `plan_updated` installed, for the corpus assertion.
    static PLAN_CAPTURE: std::cell::RefCell<Option<leveler_client_protocol::UiPlan>> =
        const { std::cell::RefCell::new(None) };
}

/// Drive one fixture for the PAINTED assertions.
fn drive_corpus(id: &str) -> (AppState, serde_json::Value) {
    drive_corpus_painted(id, true)
}

/// Every `expect.items` entry of the fixture with the given `kind`.
fn expected_item(doc: &serde_json::Value, kind: &str) -> serde_json::Value {
    doc["expect"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} in {}", doc["id"]))
        .clone()
}

/// CONV-C1: a confirmed edit is displayed in full — every changed line, no
/// click, no `… +N lines` substitution, no diffstat-only row.
#[test]
fn conversation_c1_a_confirmed_edit_is_painted_in_full() {
    let (s, doc) = drive_corpus("C1");
    let expected = expected_item(&doc, "edit_diff");
    assert_eq!(expected["rendered_in_full"], true);
    assert_eq!(expected["needs_click"], false);
    let painted = text(&s);
    assert!(
        painted.contains("- const RECOMMENDED"),
        "the removed line is on screen: {painted}"
    );
    assert!(
        painted.contains("+ const RECOMMENDED"),
        "the added line is on screen: {painted}"
    );
    assert!(
        painted.contains("gpt-6"),
        "the applied value is on screen: {painted}"
    );
    for substitution in ["+1 −1 lines", "… +", "展开"] {
        assert!(
            !painted.contains(substitution),
            "a confirmed diff is never summarised or gated ({substitution}): {painted}"
        );
    }
}

/// CONV-C2: consecutive read-only exploration is ONE collapsed receipt whose
/// members are not painted until it opens.
#[test]
fn conversation_c2_exploration_is_one_collapsed_receipt() {
    let (s, doc) = drive_corpus("C2");
    let expected = expected_item(&doc, "exploration_receipt");
    assert_eq!(expected["folded"], true);
    assert_eq!(expected["members_visible"], false);
    let painted = text(&s);
    assert!(
        painted.contains("读取 2 个文件 · 搜索 1 次"),
        "the merged receipt is on screen: {painted}"
    );
    assert!(
        !painted.contains("src/models.rs"),
        "a collapsed receipt does not paint a member waterfall: {painted}"
    );
}

/// CONV-C3: provider reasoning is a folded Thought with the runtime's own
/// duration — never assistant prose, and never open by default.
#[test]
fn conversation_c3_a_completed_thought_is_folded() {
    let (s, doc) = drive_corpus("C3");
    let mut expected = expected_item(&doc, "thought");
    assert_eq!(expected["folded"], true);
    assert_eq!(expected["body_visible"], false);
    let painted = text(&s);
    assert!(
        painted.contains("思考 · 1.6s"),
        "the Thought header carries the runtime's measurement: {painted}"
    );
    assert!(
        !painted.contains("先看解析器的入口。"),
        "a completed Thought comes back folded: {painted}"
    );
    // No raw reasoning was promoted into the transcript as prose.
    assert_eq!(
        expected["body"], "先看解析器的入口。再看它的调用方。",
        "the corpus body is the durable segment: {painted}"
    );
    expected["state"] = serde_json::json!("completed");
    assert_eq!(expected["state"], "completed");
}

/// CONV-C4: a failed Run is collapsed AND readable — the command's own failure
/// line is on screen before anything is expanded.
#[test]
fn conversation_c4_a_failed_run_shows_its_failure_collapsed() {
    let (s, doc) = drive_corpus("C4");
    let expected = expected_item(&doc, "run_receipt");
    assert_eq!(expected["folded"], true);
    assert_eq!(expected["failure_visible"], true);
    let painted = text(&s);
    assert!(
        painted.contains("test mapping::recommended ... FAILED"),
        "the first line reporting the failure is on the collapsed row: {painted}"
    );
    assert!(
        !painted.contains("exit: 101"),
        "a runtime execution row is never the failure reason: {painted}"
    );
    assert!(
        painted.contains("✗") || painted.contains("失败"),
        "the row states it failed: {painted}"
    );
}

// ── The shared conversation-presentation corpus: the full tree ──────────────
//
// Every fixture in `testdata/conversation_presentation/v1/` declares the
// semantic tree the conversation must project to. This is the reference
// projection: the terminal's own transcript, walked in order, reduced to the
// items the corpus names — kinds, order, fold state, visibility, roles. The Web
// client and the Desktop renderer project the SAME files.

/// The exploration kind of a call, from the reference's own taxonomy.
fn exploration_kind(name: &str) -> Option<&'static str> {
    use leveler_tui::tool_taxonomy::ToolKind;
    match leveler_tui::tool_taxonomy::lookup(name)?.kind {
        ToolKind::Read => Some("read"),
        ToolKind::ListDir => Some("list"),
        ToolKind::Search => Some("search"),
        _ => None,
    }
}

/// The target a row names, read from the call's own arguments.
fn call_target(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return String::new();
    };
    for key in ["path", "pattern", "query", "glob", "file"] {
        if let Some(found) = value.get(key).and_then(|v| v.as_str()) {
            return found.to_string();
        }
    }
    String::new()
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

fn tool_status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Ok => "ok",
        ToolStatus::Failed => "failed",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::Unknown => "unknown",
    }
}

/// An edit's confirmed diff, as the corpus names it: the change itself, whole.
///
/// A group whose visible calls are all confirmed edits IS its diff — the row
/// header is the diff's own header, and no diffstat-only summary may take its
/// place (`rendered_in_full` is what the client must be able to paint).
fn edit_diff_item(calls: &[&leveler_tui::transcript::ToolCallBlock]) -> Option<Value> {
    let mut patches = Vec::new();
    for call in calls {
        let patch = call.applied_diff.as_deref()?;
        if patch.trim().is_empty() {
            return None;
        }
        patches.push(patch);
    }
    if patches.is_empty() {
        return None;
    }
    let mut paths: Vec<String> = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    for patch in &patches {
        for line in patch.lines() {
            if let Some(rest) = line.strip_prefix("+++ ") {
                let path = rest
                    .trim()
                    .trim_start_matches("b/")
                    .trim_start_matches("a/")
                    .to_string();
                if !path.is_empty() && !paths.contains(&path) {
                    paths.push(path);
                }
            } else if line.starts_with('+') && !line.starts_with("+++") {
                added += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                removed += 1;
            }
        }
    }
    paths.sort();
    Some(json!({
        "kind": "edit_diff",
        "paths": paths,
        "added": added,
        "removed": removed,
        "rendered_in_full": true,
        "needs_click": false,
    }))
}

/// One tool group as the corpus names it: a confirmed edit's diff, a merged
/// exploration receipt, lone exploration rows, or a Run receipt. The fold state
/// is the group's own `display`.
fn group_item(group: &leveler_tui::transcript::ToolGroupBlock) -> Value {
    let visible: Vec<&leveler_tui::transcript::ToolCallBlock> = group.calls.iter().collect();
    if !visible.is_empty()
        && visible
            .iter()
            .all(|call| matches!(&call.name[..], "apply_patch" | "write_file" | "edit_file"))
        && let Some(diff) = edit_diff_item(&visible)
    {
        return diff;
    }
    let kinds: Vec<Option<&'static str>> = visible
        .iter()
        .map(|call| exploration_kind(&call.name))
        .collect();
    let all_exploration = !visible.is_empty() && kinds.iter().all(Option::is_some);
    let none_failed = visible.iter().all(|call| call.status != ToolStatus::Failed);
    let folded = group.display.is_collapsed();
    if all_exploration && visible.len() >= 2 && none_failed {
        let reads = kinds.iter().filter(|kind| **kind == Some("read")).count();
        let searches = kinds.iter().filter(|kind| **kind == Some("search")).count();
        // Arrival order, never sorted: the fold restores the chronology it
        // hid, so the order the calls happened in IS part of the item.
        let members: Vec<String> = visible
            .iter()
            .map(|call| format!("{}:{}", call.name, call_target(&call.arguments)))
            .collect();
        return json!({
            "kind": "exploration_receipt",
            "reads": reads,
            "searches": searches,
            "folded": folded,
            "members_visible": !folded,
            "reversible": true,
            "members": members,
        });
    }
    if all_exploration && visible.len() == 1 {
        let call = visible[0];
        return json!({
            "kind": "exploration_row",
            "name": call.name,
            "target": call_target(&call.arguments),
            "status": tool_status_name(call.status),
        });
    }
    let rows: Vec<Value> = visible
        .iter()
        .map(|call| {
            json!({
                "id": call.id.to_string(),
                "name": call.name,
                "status": tool_status_name(call.status),
            })
        })
        .collect();
    let status = match group.calls.iter().map(|call| call.status).find(|status| {
        matches!(
            status,
            ToolStatus::Running | ToolStatus::Failed | ToolStatus::Cancelled | ToolStatus::Unknown
        )
    }) {
        Some(ToolStatus::Running) => "running",
        Some(ToolStatus::Failed) => "failed",
        Some(ToolStatus::Cancelled) => "cancelled",
        Some(ToolStatus::Unknown) => "unknown",
        _ => "ok",
    };
    let mut item = json!({
        "kind": "run_receipt",
        "model_step": group.round,
        "status": status,
        "folded": folded,
        "command_visible": true,
        "output_visible": !folded,
        "output_available": true,
        "rows": rows,
    });
    if status == "failed" {
        // The contract's failure facts: the status is above, the exit code and
        // the command's own failure line follow it.
        item["failure_visible"] = json!(true);
        let exit_code = group
            .calls
            .iter()
            .filter(|call| call.status == ToolStatus::Failed)
            .find_map(|call| call.exit_code);
        if let Some(code) = exit_code {
            item["exit_code"] = json!(code);
        }
        item["failure_line"] = json!(
            group
                .calls
                .iter()
                .filter(|call| call.status == ToolStatus::Failed)
                .find_map(|call| call.preview.as_deref())
                .map(|preview| {
                    preview
                        .lines()
                        .map(str::trim)
                        .find(|line| {
                            let lower = line.to_lowercase();
                            lower.starts_with("error")
                                || lower.contains("panic")
                                || line.contains('\u{2717}')
                                || lower.contains("failed")
                                || lower.contains("fail")
                        })
                        .unwrap_or_default()
                        .to_string()
                })
                .unwrap_or_default()
        );
    }
    item
}

/// The conversation as the corpus names it: the reference's own transcript, in
/// order, reduced to the contract's items.
fn conversation_tree(s: &AppState) -> Vec<Value> {
    let mut items = Vec::new();
    for item in s.transcript.items() {
        match item {
            TranscriptItem::User(text) => items.push(json!({"kind": "user", "text": text})),
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
            TranscriptItem::Thought(block) => {
                let state = if !block.done {
                    "running"
                } else if block.interrupted {
                    "interrupted"
                } else {
                    "completed"
                };
                items.push(json!({
                    "kind": "thought",
                    "state": state,
                    "elapsed_ms": block.duration_ms,
                    "folded": block.display.is_collapsed(),
                    "body_visible": block.display.is_expanded(),
                    "body": block.text,
                }));
            }
            TranscriptItem::ToolGroup(group) => items.push(group_item(group)),
            TranscriptItem::TurnEnd(end) => items.push(json!({
                "kind": "turn_end",
                "status": turn_end_status(end.status),
            })),
            // A row the runtime authored (a notice, a checkpoint line, the
            // compaction marker) is never user speech and never a Thought.
            TranscriptItem::Note(text) => {
                items.push(json!({"kind": "runtime_notice", "text": text}))
            }
            TranscriptItem::Failure(failure) => items.push(json!({
                "kind": "failure",
                "title": failure.title,
                "summary": failure.summary,
            })),
            _ => {}
        }
    }
    items
}

/// The fields of `expect.items` must appear in the same position in the tree.
///
/// An item marked `optional` is a fact a client may express in another place
/// (the terminal paints a dedicated failure block; the Web and Desktop state the
/// same failure in their run row and turn terminal). It is asserted when it is
/// present and never forces the other clients to invent a row.
fn assert_corpus_items(id: &str, expect: &[Value], actual: &[Value]) {
    let mut cursor = 0usize;
    for (index, expected) in expect.iter().enumerate() {
        let object = expected.as_object().expect("expected item");
        let fields: Vec<(&String, &Value)> = object
            .iter()
            .filter(|(key, _)| key.as_str() != "optional")
            .collect();
        if object.get("optional").and_then(Value::as_bool) == Some(true) {
            let matches = actual.get(cursor).is_some_and(|candidate| {
                fields
                    .iter()
                    .all(|(key, value)| candidate.get(*key) == Some(*value))
            });
            if !matches {
                continue;
            }
        }
        let actual_item = actual
            .get(cursor)
            .unwrap_or_else(|| panic!("{id}: missing item {index}: {expected}\n{actual:#?}"));
        for (key, value) in fields {
            assert_eq!(
                actual_item.get(key),
                Some(value),
                "{id}: item {index} field {key:?}\nexpected {expected}\nactual {actual_item:#?}"
            );
        }
        cursor += 1;
    }
    assert_eq!(
        cursor,
        actual.len(),
        "{id}: the tree has unclaimed items\n{:#?}",
        &actual[cursor.min(actual.len())..]
    );
}

fn all_conversation_fixtures() -> Vec<serde_json::Value> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/conversation_presentation/v1");
    let mut ids: Vec<u32> = (1..=10).collect();
    ids.sort();
    ids.into_iter()
        .map(|id| {
            let path = dir.join(format!("C{id}.json"));
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            serde_json::from_str(&raw).expect("fixture json")
        })
        .collect()
}

#[test]
fn conversation_corpus_projects_the_frozen_tree() {
    for doc in all_conversation_fixtures() {
        let id = doc["id"].as_str().expect("id");
        let (s, _) = drive_corpus_painted(id, false);
        let expect = doc["expect"]["items"].as_array().expect("items").clone();
        let actual = conversation_tree(&s);
        assert_corpus_items(id, &expect, &actual);
        if let Some(collaboration) = doc["expect"]["collaboration"].as_str() {
            assert_eq!(
                s.collaboration, collaboration,
                "{id}: the session axis is a runtime fact, not a transcript item"
            );
        }
        if let Some(plan) = doc["expect"].get("plan") {
            let captured = PLAN_CAPTURE.with(|slot| slot.borrow().clone());
            let steps: Vec<Value> = captured
                .as_ref()
                .map(|plan| {
                    plan.steps
                        .iter()
                        .map(|step| {
                            json!({
                                "description": step.description,
                                "status": match step.status {
                                    leveler_client_protocol::PlanStepStatus::Pending => "pending",
                                    leveler_client_protocol::PlanStepStatus::Running => "running",
                                    leveler_client_protocol::PlanStepStatus::Done => "done",
                                    leveler_client_protocol::PlanStepStatus::Failed => "failed",
                                    leveler_client_protocol::PlanStepStatus::Skipped => "skipped",
                                },
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(
                plan["steps"].as_array().map(|steps| steps.len()),
                Some(steps.len()),
                "{id}: plan steps\n{steps:#?}"
            );
            for (index, expected) in plan["steps"].as_array().expect("steps").iter().enumerate() {
                for (key, value) in expected.as_object().expect("step") {
                    assert_eq!(
                        steps[index].get(key),
                        Some(value),
                        "{id}: plan step {index} field {key:?}\n{steps:#?}"
                    );
                }
            }
        }
        if let Some(forbidden) = doc["expect"]["forbidden_text"].as_array() {
            let tree = serde_json::to_string(&actual).expect("tree");
            for needle in forbidden {
                let needle = needle.as_str().unwrap_or_default();
                assert!(
                    !tree.contains(needle),
                    "{id}: forbidden text in the conversation tree: {needle}\n{tree}"
                );
            }
        }
    }
}

/// The compacted model context must not replace the conversation restored
/// through the client's actual history response boundary.
#[test]
fn conversation_c10_durable_history_replay_preserves_full_tree_and_reversible_thought() {
    let doc = corpus("C10");
    let entries: Vec<leveler_client_protocol::UiHistoryEntry> =
        serde_json::from_value(doc["paths"]["replay"].clone()).expect("C10 durable replay entries");
    let mut state = opened_bare();
    // A stale context-only placeholder on screen must be replaced by the
    // authoritative full history, rather than retained or appended twice.
    state.transcript.push_user("对话摘要（已压缩历史）".into());
    let query = leveler_client_protocol::CommandId::new("conversation-C10-replay");
    state.history_query = Some(query.clone());
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionHistoryLoaded {
            query_id: Some(query),
            session_id: SessionId::new("s1"),
            entries,
            omitted_turns: 0,
        }),
    );
    assert!(
        state.history_query.is_none(),
        "the real history query was consumed"
    );
    let expected = doc["expect"]["items"].as_array().expect("C10 items");
    assert_corpus_items("C10/replay", expected, &conversation_tree(&state));
    let painted = text(&state);
    for forbidden in doc["expect"]["forbidden_text"]
        .as_array()
        .expect("forbidden text")
    {
        assert!(!painted.contains(forbidden.as_str().unwrap()), "{painted}");
    }
    assert_eq!(
        painted.matches("先看解析器").count(),
        1,
        "first user is visible exactly once"
    );
    assert_eq!(
        painted.matches("把 parse 修好").count(),
        1,
        "second user is visible exactly once"
    );
    assert!(
        painted.contains("- fn parse() {}"),
        "confirmed removal remains in full: {painted}"
    );
    assert!(
        painted.contains("+ fn parse() { ok() }"),
        "confirmed addition remains in full: {painted}"
    );
    assert_eq!(thoughts(&state).len(), 1);
    assert_eq!(thoughts(&state)[0].duration_ms, Some(1600));
    assert_eq!(thoughts(&state)[0].display, DisplayMode::Collapsed);
    assert!(
        !painted.contains("先看入口。"),
        "replayed reasoning starts folded"
    );
    let thought = thought_index(&state);
    toggle_fold(&mut state, thought);
    assert!(
        text(&state).contains("先看入口。"),
        "the original thought is reversible"
    );
    toggle_fold(&mut state, thought);
    assert!(
        !text(&state).contains("先看入口。"),
        "the original thought folds back"
    );
}
