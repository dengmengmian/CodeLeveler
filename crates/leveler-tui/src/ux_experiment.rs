//! Experimental UX harness (test-only).
//!
//! This module is the single owner of the `Experimental UX Phase` comparison
//! machinery: deterministic transcripts (T1–T6), a thread-local *presentation
//! policy* the production renderers read only under `cfg(test)`, headless frame
//! dumps, and the task metrics the phase is judged by.
//!
//! It is compiled ONLY for unit tests (`#[cfg(test)] mod ux_experiment;` in
//! `lib.rs`), so none of it ships. The production `#[cfg(test)]` seams that
//! call it (`activity_stream`, `render::transcript_lines`, `conversation::build`)
//! are removed or turned into real product logic at the end of the phase.
//!
//! Run:
//! ```text
//! cargo test -p leveler-tui --lib ux_experiment -- --ignored --nocapture
//! ```
//!
//! Output frames are written to `/tmp/leveler-ux-exp/` (never committed).

use leveler_client_protocol::{
    MessageId, PermissionProfile, RuntimeEvent, SessionId, ToolCallId, UiMessage, UiRole,
    UiSessionSnapshot,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::text::Line;

use crate::action::Action;
use crate::reducer::reduce;
use crate::state::{AppState, Boot};
use crate::theme::Theme;

// ───────────────────────────────────────────────────────────────────────────
// Presentation policy (the A/B switch)
// ───────────────────────────────────────────────────────────────────────────

/// Presentation-policy override used by the retained UX harness. The diff-cap
/// knob is the one the implemented I-03 change needs to be compared against
/// fixed budgets; the rejected folding policies left no seam behind.
pub(crate) mod policy {
    use std::cell::Cell;

    thread_local! {
        static DIFF_CAP: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub(crate) fn set_diff_preview_cap(cap: Option<usize>) {
        DIFF_CAP.with(|c| c.set(cap));
    }

    /// The preview row budget the diff renderer should use. `default_cap` is
    /// the production value derived from the viewport; the experiment overrides
    /// it to compare fixed caps against the responsive one.
    pub(crate) fn diff_preview_cap(default_cap: usize) -> usize {
        DIFF_CAP.with(|c| c.get()).unwrap_or(default_cap)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Deterministic transcripts
// ───────────────────────────────────────────────────────────────────────────

const OUT_DIR: &str = "/tmp/leveler-ux-exp";

fn opened(goal: &str) -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("uxexp"),
            user: "麻凡".into(),
            version: "ux-exp".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 1_048_576,
            locale: crate::i18n::Locale::Zh,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    );
    let snap = UiSessionSnapshot {
        id: SessionId::new("uxexp"),
        repository: "~/Develop/navsvc".into(),
        goal: goal.into(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/deepseek-flash"),
        mode: PermissionProfile::Assisted,
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
        reasoning: None,
        work_profile: None,
        collaboration: None,
        children: Vec::new(),
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: goal.into(),
                ordinal: None,
                kind: None,
                images: 0,
            },
        }),
    );
    s
}

fn say(s: &mut AppState, id: &str, text: &str) {
    let m = MessageId::new(id);
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: m.clone(),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: m.clone(),
            delta: text.into(),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: m }),
    );
}

fn tool(s: &mut AppState, id: &str, name: &str, args: &str, ok: bool, preview: &str, ms: u64) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: name.into(),
            arguments: args.into(),
            parallel: false,
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new(id),
            ok,
            preview: preview.into(),
            duration_ms: ms,
            applied_diff: None,
        }),
    );
}

fn read(s: &mut AppState, id: &str, path: &str, lines: usize) {
    tool(
        s,
        id,
        "read_file",
        &serde_json::json!({ "path": path }).to_string(),
        true,
        &format!("{path} 共 {lines} 行"),
        2,
    );
}

fn search(s: &mut AppState, id: &str, pattern: &str) {
    tool(
        s,
        id,
        "grep",
        &serde_json::json!({ "pattern": pattern }).to_string(),
        true,
        "internal/billing/refund.go:41\ninternal/api/handler.go:88",
        14,
    );
}

/// A unified diff with `n` changed lines, the shape an `apply_patch` reports.
fn unified_diff(path: &str, n: usize) -> String {
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    let mut i = 0usize;
    while i < n {
        out.push_str(&format!(
            "@@ -{},6 +{},6 @@ fn section_{i}\n",
            i + 10,
            i + 10
        ));
        out.push_str(" context before\n");
        out.push_str(&format!("-    let value = old_call_{i}();\n"));
        out.push_str(&format!("+    let value = new_call_{i}();\n"));
        out.push_str(" context after\n");
        i += 1;
    }
    out
}

fn edit(s: &mut AppState, id: &str, path: &str, changes: usize) {
    let applied = unified_diff(path, changes);
    let body = applied
        .lines()
        .filter(|l| l.starts_with('+') || l.starts_with('-'))
        .filter(|l| !l.starts_with("+++") && !l.starts_with("---"))
        .collect::<Vec<_>>()
        .join("\n");
    let patch = format!("*** Begin Patch\n*** Update File: {path}\n@@\n{body}\n*** End Patch");
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: "apply_patch".into(),
            arguments: serde_json::json!({ "patch": patch }).to_string(),
            parallel: false,
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            exit_code: None,
            stop: None,
            id: ToolCallId::new(id),
            ok: true,
            preview: format!("patched {path}"),
            duration_ms: 9,
            applied_diff: Some(applied),
        }),
    );
}

fn run_test(s: &mut AppState, id: &str, ok: bool, preview: &str) {
    tool(
        s,
        id,
        "run_command",
        r#"{"program":"cargo","args":["test","--quiet"]}"#,
        ok,
        preview,
        1438,
    );
}

fn turn_end(s: &mut AppState) {
    reduce(s, Action::Runtime(RuntimeEvent::TurnCompleted));
    // A following user turn, so the tool group is sealed and the answer is
    // classified as a Final rather than containing the live group.
    s.transcript.finalize_in_flight();
}

fn long_final(paragraphs: usize) -> String {
    let mut out = String::from("## 总结\n\n");
    for i in 0..paragraphs {
        out.push_str(&format!(
            "### 变更点 {i}\n\n\
             修改了 `crate::section_{i}` 的调用链，把旧的 `old_call_{i}()` 换成 `new_call_{i}()`，\
             并补充了边界处理与错误传播。这样 `handler_{i}` 在空列表时不再 panic。\n\n\
             - 覆盖了空输入与单元素输入\n- 保留了原有的失败语义\n- 不改变公开 API\n\n"
        ));
    }
    out.push_str("## 验证\n\n`cargo test --quiet` 全部通过。\n\n## 剩余风险\n\n未覆盖并发路径。\n");
    out
}

// ── T1 exploration-heavy ───────────────────────────────────────────────────

fn build_t1() -> AppState {
    let mut s = opened("排查整个 crate 的溢出/panic 风险，并修复 mean");
    say(&mut s, "p1", "先并行读取核心文件，定位可能溢出的算术路径。");
    for i in 0..10 {
        read(
            &mut s,
            &format!("r{i}"),
            &format!("src/module_{i}.rs"),
            30 + i,
        );
    }
    for i in 0..4 {
        search(&mut s, &format!("g{i}"), "sum(");
    }
    say(
        &mut s,
        "p2",
        "已经确认溢出点集中在 stats 汇总路径，准备修复。",
    );
    edit(&mut s, "e1", "src/stats.rs", 4);
    run_test(&mut s, "t1", true, "test result: ok. 12 passed; 0 failed");
    say(
        &mut s,
        "f1",
        "已修复溢出：用饱和加法替换直接相加，并补了测试。",
    );
    turn_end(&mut s);
    s
}

// ── T2 large edits ─────────────────────────────────────────────────────────

fn build_t2() -> AppState {
    let mut s = opened("把 refund 的新参数传播到所有调用点");
    say(&mut s, "p1", "先读取要改的三个文件。");
    read(&mut s, "r0", "internal/billing/refund.go", 120);
    edit(&mut s, "e1", "internal/billing/refund.go", 32);
    edit(&mut s, "e2", "internal/api/handler.go", 12);
    edit(&mut s, "e3", "internal/api/router.go", 3);
    run_test(&mut s, "t1", true, "ok");
    say(&mut s, "f1", "三个文件已更新，测试通过。");
    turn_end(&mut s);
    s
}

/// T2 stopped right after the edits: the edit group is the LIVE edge, which is
/// the moment the I-03 question is actually asked ("is the edit I just made
/// still on screen?").
fn build_t2_live() -> AppState {
    let mut s = opened("把 refund 的新参数传播到所有调用点");
    say(&mut s, "p1", "先读取要改的三个文件。");
    read(&mut s, "r0", "internal/billing/refund.go", 120);
    edit(&mut s, "e1", "internal/billing/refund.go", 32);
    edit(&mut s, "e2", "internal/api/handler.go", 12);
    edit(&mut s, "e3", "internal/api/router.go", 3);
    s.transcript.finalize_in_flight();
    s
}

// ── T3 / T4 long Final ─────────────────────────────────────────────────────

fn build_t3() -> AppState {
    let mut s = opened("实现新的用法归一化并给出结构化总结");
    say(&mut s, "p1", "先读取 resolver 与测试。");
    read(&mut s, "r0", "src/resolver.rs", 90);
    edit(&mut s, "e1", "src/resolver.rs", 6);
    run_test(&mut s, "t1", true, "ok");
    say(&mut s, "f1", &long_final(4));
    turn_end(&mut s);
    s
}

fn build_t4() -> AppState {
    let mut s = opened("跨四个文件重构并给出详尽总结");
    say(&mut s, "p1", "读取文件。");
    for i in 0..4 {
        read(&mut s, &format!("r{i}"), &format!("src/part_{i}.rs"), 80);
    }
    for i in 0..4 {
        edit(&mut s, &format!("e{i}"), &format!("src/part_{i}.rs"), 4);
    }
    run_test(&mut s, "t1", true, "ok");
    say(&mut s, "f1", &long_final(10));
    turn_end(&mut s);
    s
}

// ── T5 long Progress ───────────────────────────────────────────────────────

fn progress_paragraph(tag: &str) -> String {
    let mut out = String::new();
    for i in 0..4 {
        out.push_str(&format!(
            "{tag}：第 {i} 点说明——我检查了调用链、错误传播和边界条件，确认这一层没有隐藏的全局状态，\
             并计划在下一步把结果落到具体文件。\n\n"
        ));
    }
    out
}

fn build_t5() -> AppState {
    let mut s = opened("分三步分析并实现");
    say(&mut s, "p1", &progress_paragraph("阶段一"));
    for i in 0..3 {
        read(&mut s, &format!("r{i}"), &format!("src/step_a_{i}.rs"), 40);
    }
    say(&mut s, "p2", &progress_paragraph("阶段二"));
    for i in 0..3 {
        search(&mut s, &format!("g{i}"), "resolver");
    }
    say(&mut s, "p3", &progress_paragraph("阶段三"));
    edit(&mut s, "e1", "src/resolver.rs", 4);
    run_test(&mut s, "t1", true, "ok");
    say(&mut s, "f1", "三个阶段完成，测试通过。");
    turn_end(&mut s);
    s
}

// ── T6 failure ─────────────────────────────────────────────────────────────

fn build_t6() -> AppState {
    let mut s = opened("加一个校验函数并确保测试通过");
    say(&mut s, "p1", "先读取现有实现。");
    read(&mut s, "r0", "src/validate.rs", 60);
    edit(&mut s, "e1", "src/validate.rs", 3);
    say(&mut s, "p2", "先跑一遍测试确认新校验的行为。");
    run_test(
        &mut s,
        "t1",
        false,
        "error[E0308]: mismatched types\n  --> src/validate.rs:42:9\n   |\n42 |     return check(value)\n   |            ^^^^^^^^^^^ expected `bool`, found `Result<bool, Error>`\n",
    );
    say(
        &mut s,
        "p3",
        "测试失败：校验函数返回了 Result，调用点仍然按 bool 使用。改为显式处理错误分支。",
    );
    edit(&mut s, "e2", "src/validate.rs", 2);
    run_test(&mut s, "t2", true, "test result: ok. 9 passed; 0 failed");
    say(&mut s, "f1", "已完成：修正错误传播后测试通过。");
    turn_end(&mut s);
    s
}

// ───────────────────────────────────────────────────────────────────────────
// Frame dump + metrics
// ───────────────────────────────────────────────────────────────────────────

fn frame_text(state: &mut AppState, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| crate::render::render(f, state)).unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

fn line_plain(l: &Line<'_>) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Per-item (start_line, line_count) in the built conversation, plus the
/// canonical item vector. Validated against the real build length.
fn item_ranges(state: &AppState, width: usize) -> Vec<(usize, usize)> {
    let items = state.transcript.items();
    let theme = &state.theme;
    let t = state.t();
    let mut ranges = Vec::with_capacity(items.len());
    let mut line = 0usize;
    for (i, item) in items.iter().enumerate() {
        if i > 0 && crate::render::items_need_gap(&items[i - 1], item) {
            line += 1;
        }
        let start = line;
        let n = crate::render::item_render(item, theme, width, false, t).len();
        ranges.push((start, n));
        line += n;
    }
    ranges
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Metrics {
    width: usize,
    height: u16,
    viewport: usize,
    total: usize,
    max_scroll: usize,
    scroll: usize,
    final_anchor: usize,
    final_visible: bool,
    final_lines: usize,
    tool_lines: usize,
    tool_groups: usize,
    progress_lines: usize,
    last_edit_anchor: usize,
    last_edit_lines: usize,
    /// Absolute line of the LAST edited file's head row inside the last edit
    /// group (what "the edit I just made" means once several edits merge).
    last_edit_head: usize,
    last_progress_anchor: usize,
    edit_head_visible: bool,
    prev_progress_visible: bool,
    diff_content_visible: usize,
    folded_runs: usize,
}

fn measure(state: &mut AppState, w: u16, h: u16) -> Metrics {
    // Render first: this publishes the authoritative viewport rect and builds
    // the frame through the same path production uses.
    let _ = frame_text(state, w, h);
    let width = crate::conversation::geometry::content_width(state);
    let viewport = crate::conversation::geometry::viewport_height(state);
    let lines = state.conversation_lines(width);
    let total = lines.len();
    let max_scroll = crate::conversation::geometry::max_scroll(total, viewport);
    let follow = state.conv.auto_scroll && !crate::splash::conversation_is_empty(state);
    let scroll =
        crate::conversation::geometry::effective_scroll(state.conv.scroll, follow, total, viewport);
    let final_anchor = state.final_answer_anchor(width).unwrap_or(0);
    let final_visible = final_anchor >= scroll && final_anchor < scroll + viewport.max(1);
    let ranges = item_ranges(state, width);
    let items = state.transcript.items();

    let final_lines = items
        .iter()
        .rposition(|it| {
            matches!(it, crate::transcript::TranscriptItem::Assistant(b) if b.kind == crate::transcript::AssistantKind::Final)
        })
        .map(|i| ranges[i].1)
        .unwrap_or(0);
    let tool_lines = items
        .iter()
        .enumerate()
        .filter(|(_, it)| matches!(it, crate::transcript::TranscriptItem::ToolGroup(_)))
        .map(|(i, _)| ranges[i].1)
        .sum();
    let tool_groups = items
        .iter()
        .filter(|it| matches!(it, crate::transcript::TranscriptItem::ToolGroup(_)))
        .count();
    let progress_lines = items
        .iter()
        .enumerate()
        .filter(|(_, it)| {
            matches!(it, crate::transcript::TranscriptItem::Assistant(b) if b.kind == crate::transcript::AssistantKind::Progress)
        })
        .map(|(i, _)| ranges[i].1)
        .sum();

    // Last tool group that owns an edit call.
    let last_edit_item = items.iter().rposition(|it| match it {
        crate::transcript::TranscriptItem::ToolGroup(g) => g
            .calls
            .iter()
            .any(|c| c.applied_diff.is_some() || c.name == "apply_patch" || c.name == "write_file"),
        _ => false,
    });
    let (last_edit_anchor, last_edit_lines) = last_edit_item
        .map(|i| (ranges[i].0, ranges[i].1))
        .unwrap_or((0, 0));
    // The most recent edited file's head row: scan the last edit group for the
    // edit action label and take the last match.
    let last_edit_head = last_edit_item
        .map(|i| {
            let (start, n) = ranges[i];
            (start..start + n)
                .rev()
                .find(|row| {
                    lines
                        .get(*row)
                        .is_some_and(|l| is_edit_head(&line_plain(l)))
                })
                .unwrap_or(start)
        })
        .unwrap_or(0);
    let last_progress_item = items
        .iter()
        .enumerate()
        .rev()
        .find(|(i, it)| {
            matches!(it, crate::transcript::TranscriptItem::Assistant(b) if b.kind == crate::transcript::AssistantKind::Progress)
                && *i < last_edit_item.unwrap_or(items.len())
        })
        .map(|(i, _)| i);
    let last_progress_anchor = last_progress_item.map(|i| ranges[i].0).unwrap_or(0);

    let visible = |line: usize| line < total && line >= max_scroll;
    let edit_head_visible = visible(last_edit_head);
    let prev_progress_visible = visible(last_progress_anchor);

    // Diff content rows inside the last edit item that are on screen: rows a
    // reader can tell a change from (`│ +` / `│ -`), not the head or the fold
    // marker.
    let mut diff_content_visible = 0usize;
    if let Some(i) = last_edit_item {
        let (start, n) = ranges[i];
        for row in start..start + n {
            let Some(l) = lines.get(row) else { continue };
            if is_diff_change_row(&line_plain(l)) && visible(row) {
                diff_content_visible += 1;
            }
        }
    }

    let folded_runs = lines
        .iter()
        .map(line_plain)
        .filter(|t| t.contains("个目标") && t.contains("失败"))
        .count();

    Metrics {
        width,
        height: h,
        viewport,
        total,
        max_scroll,
        scroll,
        final_anchor,
        final_visible,
        final_lines,
        tool_lines,
        tool_groups,
        progress_lines,
        last_edit_anchor,
        last_edit_lines,
        last_edit_head,
        last_progress_anchor,
        edit_head_visible,
        prev_progress_visible,
        diff_content_visible,
        folded_runs,
    }
}

/// Whether a plain conversation line is an edit node's head row.
fn is_edit_head(text: &str) -> bool {
    text.contains("编辑文件") || text.contains("写入文件") || text.contains("Edit file")
}

/// Whether a plain conversation line is a diff change row (`│ +` / `│ -`).
fn is_diff_change_row(text: &str) -> bool {
    let Some((_, after)) = text.rsplit_once('\u{2502}') else {
        return false;
    };
    let rest = after.trim_start();
    (rest.starts_with('+') || rest.starts_with('-'))
        && !rest.starts_with("+++")
        && !rest.starts_with("---")
}

fn write_dump(name: &str, frame: &str) {
    let _ = std::fs::create_dir_all(OUT_DIR);
    let path = format!("{OUT_DIR}/{name}.txt");
    let _ = std::fs::write(&path, frame);
}

/// Compact before/after frame for the report: the visible window only.
fn visible_window(state: &mut AppState, w: u16, h: u16) -> String {
    frame_text(state, w, h)
}

/// The whole built conversation (not only the visible window), so a fold high
/// above the live edge is still comparable in the dump.
fn full_text(state: &mut AppState, w: u16, h: u16) -> String {
    let _ = frame_text(state, w, h);
    let width = crate::conversation::geometry::content_width(state);
    state
        .conversation_lines(width)
        .iter()
        .map(line_plain)
        .collect::<Vec<_>>()
        .join("\n")
}

fn header<W: std::io::Write>(out: &mut W, text: &str) {
    let _ = writeln!(out, "\n===== {text} =====");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn report(name: &str) -> std::io::BufWriter<std::fs::File> {
        let _ = std::fs::create_dir_all(OUT_DIR);
        std::io::BufWriter::new(
            std::fs::File::create(format!("{OUT_DIR}/report-{name}.txt")).expect("report file"),
        )
    }

    fn reset_policies() {
        policy::set_diff_preview_cap(None);
    }

    /// I-02: tool-history density, measured on the CURRENT design. The A/B
    /// comparison (fold settled runs) was run during the phase and rejected;
    /// this test keeps the fixture that made the case and records the real
    /// target count so a future change cannot claim folding is free.
    #[test]
    #[ignore = "ux experiment harness"]
    fn i02_tool_history_density() {
        let mut out = report("i02");
        reset_policies();
        for (label, build) in [("T1", build_t1 as fn() -> AppState), ("T2", build_t2)] {
            header(&mut out, &format!("I-02 {label} / 80x24"));
            let mut a = build();
            let ma = measure(&mut a, 80, 24);
            write_dump(&format!("i02-{label}-80x24"), &full_text(&mut a, 80, 24));
            let _ = writeln!(out, "current: {ma:?}");
            let _ = writeln!(
                out,
                "  tool groups={} tool lines={} (fold would replace each target row with 1 summary line)",
                ma.tool_groups, ma.tool_lines
            );
            assert!(
                ma.folded_runs == 0,
                "the shipped UI never folds a run's target list"
            );
        }
        reset_policies();
    }

    /// I-03: diff preview density. T2.
    #[test]
    #[ignore = "ux experiment harness"]
    fn i03_diff_preview() {
        let mut out = report("i03");
        reset_policies();
        for (w, h) in [(80u16, 24u16), (100, 30), (120, 40)] {
            header(
                &mut out,
                &format!("I-03 T2 / {w}x{h} (measured at the edit live edge)"),
            );
            // Variant A — the pre-experiment fixed 24-row cap, forced.
            policy::set_diff_preview_cap(Some(24));
            let mut a = build_t2_live();
            let ma = measure(&mut a, w, h);
            write_dump(
                &format!("i03-live-A-{w}x{h}"),
                &visible_window(&mut a, w, h),
            );
            let _ = writeln!(out, "A 24:   {ma:?}");

            policy::set_diff_preview_cap(Some(12));
            let mut b = build_t2_live();
            let mb = measure(&mut b, w, h);
            write_dump(
                &format!("i03-live-B-{w}x{h}"),
                &visible_window(&mut b, w, h),
            );
            let _ = writeln!(out, "B 12:   {mb:?}");

            policy::set_diff_preview_cap(Some(6));
            let mut c = build_t2_live();
            let mc = measure(&mut c, w, h);
            write_dump(
                &format!("i03-live-C-{w}x{h}"),
                &visible_window(&mut c, w, h),
            );
            let _ = writeln!(out, "C 6:    {mc:?}");

            // Variant D — responsive: a fraction of the effective viewport.
            policy::set_diff_preview_cap(None);
            let mut probe = build_t2_live();
            let viewport = measure(&mut probe, w, h).viewport;
            for (tag, formula) in [
                ("D1 viewport/2", (viewport / 2).clamp(6, 24)),
                ("D2 viewport-4", viewport.saturating_sub(4).clamp(6, 24)),
                ("D3 viewport-3", viewport.saturating_sub(3).clamp(6, 24)),
            ] {
                policy::set_diff_preview_cap(Some(formula));
                let mut d = build_t2_live();
                let md = measure(&mut d, w, h);
                write_dump(
                    &format!("i03-live-{tag}-{w}x{h}"),
                    &visible_window(&mut d, w, h),
                );
                let _ = writeln!(
                    out,
                    "{tag} = {formula}: head_visible={} change_rows_visible={} total={}",
                    md.edit_head_visible, md.diff_content_visible, md.total
                );
            }
            // PRODUCTION candidate: quantized viewport budget (cap = None uses
            // the value build_conversation derives from the viewport).
            policy::set_diff_preview_cap(None);
            let mut prod = build_t2_live();
            let mprod = measure(&mut prod, w, h);
            write_dump(
                &format!("i03-live-PROD-{w}x{h}"),
                &visible_window(&mut prod, w, h),
            );
            let _ = writeln!(
                out,
                "PROD quantized: viewport={} head_visible={} change_rows_visible={} total={} {mprod:?}",
                mprod.viewport, mprod.edit_head_visible, mprod.diff_content_visible, mprod.total
            );
            reset_policies();

            // Full-turn density (Final included): how much does the preview
            // budget leave in history once the turn is done?
            policy::set_diff_preview_cap(Some(24));
            let mut fa = build_t2();
            let mfa = measure(&mut fa, w, h);
            policy::set_diff_preview_cap(None);
            let mut fb = build_t2();
            let mfb = measure(&mut fb, w, h);
            reset_policies();
            let _ = writeln!(
                out,
                "  full-turn total lines: A24={} PROD={} (saved {})",
                mfa.total,
                mfb.total,
                mfa.total.saturating_sub(mfb.total)
            );
        }
        reset_policies();
    }

    /// I-04: Final navigation. T3 (40-line Final) + T4 (110-line Final).
    #[test]
    #[ignore = "ux experiment harness"]
    fn i04_final_navigation() {
        let mut out = report("i04");
        reset_policies();
        for (label, build) in [("T3", build_t3 as fn() -> AppState), ("T4", build_t4)] {
            let mut s = build();
            let m = measure(&mut s, 80, 24);
            header(&mut out, &format!("I-04 {label} / 80x24"));
            let page_step = (24 / 2) as usize; // PageUp uses size.1/2
            let pageups = m
                .max_scroll
                .saturating_sub(m.final_anchor)
                .div_ceil(page_step);
            let _ = writeln!(
                out,
                "baseline: final_anchor={} final_lines={} max_scroll={} scroll={} final_visible={} => {pageups} PageUp to reach Final start",
                m.final_anchor, m.final_lines, m.max_scroll, m.scroll, m.final_visible
            );
            write_dump(
                &format!("i04-{label}-live-80x24"),
                &visible_window(&mut s, 80, 24),
            );

            // Variant B — the real Ctrl+G jump (production reducer path).
            reduce(
                &mut s,
                Action::Key(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('g'),
                    crossterm::event::KeyModifiers::CONTROL,
                )),
            );
            let mj = measure(&mut s, 80, 24);
            let _ = writeln!(
                out,
                "Ctrl+G:   scroll {} -> {} final_visible={} (actions: 1) {mj:?}",
                m.scroll, mj.scroll, mj.final_visible
            );
            write_dump(
                &format!("i04-{label}-jump-80x24"),
                &visible_window(&mut s, 80, 24),
            );
            assert!(
                mj.final_visible,
                "{label}: Ctrl+G must land on the Final start"
            );

            // The other Variant A inputs, for the record: page-up count.
            let _ = writeln!(out, "A PageUp: {pageups} actions");
        }
        reset_policies();
    }

    /// EXP-04: historical Progress. T5.
    #[test]
    #[ignore = "ux experiment harness"]
    fn exp04_historical_progress() {
        let mut out = report("exp04");
        reset_policies();
        header(&mut out, "EXP-04 T5 / 80x24");
        let mut a = build_t5();
        let ma = measure(&mut a, 80, 24);
        write_dump("exp04-T5-80x24", &full_text(&mut a, 80, 24));
        let _ = writeln!(out, "current: {ma:?}");
        // The A/B comparison (dim / fold long historical Progress) was run
        // during the phase and rejected: the problem was not reproduced in
        // normal dogfood, and folding has no expansion path back to the text.
        let _ = writeln!(
            out,
            "  historical Progress lines={} (kept in full: it carries the 'why')",
            ma.progress_lines
        );
        reset_policies();
    }

    /// T6: a failure's evidence must stay on the surface. The shipped UI has no
    /// folding policy, so this asserts the first error row is rendered where a
    /// reader can find it.
    #[test]
    #[ignore = "ux experiment harness"]
    fn t6_failure_evidence_is_visible() {
        let mut out = report("t6");
        reset_policies();
        header(&mut out, "T6 failure / 80x24");
        let mut a = build_t6();
        let ma = measure(&mut a, 80, 24);
        write_dump("t6-failure-80x24", &full_text(&mut a, 80, 24));
        let _ = writeln!(out, "current: {ma:?}");
        let lines: Vec<String> = a
            .conversation_lines(crate::conversation::geometry::content_width(&a))
            .iter()
            .map(line_plain)
            .collect();
        let failures = lines.iter().filter(|l| l.contains('\u{2717}')).count();
        let first_error = lines.iter().any(|l| l.contains("E0308"));
        let _ = writeln!(
            out,
            "failure rows (`✗`)={failures} first_error_present={first_error}"
        );
        assert!(failures >= 1, "the failed command keeps a `✗` row");
        assert!(first_error, "the compiler's first error line stays visible");
    }
}
