//! Large-diff performance evidence for the edit surface.
//!
//! The contract this measures is the product's: an applied diff is painted in
//! full by default, so the cost has to be understood at 100, 1k, 5k, 10k and
//! 100k rows — not hidden behind a line-count fold.
//!
//! Two things are deliberately separated so the numbers mean something:
//!
//! * Patch / file text generation happens BEFORE any timing starts, so the
//!   reported cost is the projection and the frame, not `format!`.
//! * Every size runs in a fresh `AppState`, so a "cold" build really has an
//!   empty item cache and "warm" measures the memoized path the viewport uses
//!   on every subsequent frame.
//!
//! Run with output:
//!
//! ```text
//! cargo test -p leveler-tui --test diff_perf -- --nocapture
//! ```
//!
//! Peak allocation is measured with a counting global allocator, so it is the
//! real heap delta of one build in this process (not an estimate).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{MessageId, RuntimeEvent, SessionId, ToolCallId};
use leveler_tui::action::Action;
use leveler_tui::conversation::build::conversation_line_count;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            let live = LIVE.fetch_add(new_size, Ordering::Relaxed) + new_size;
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        out
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const PATH: &str = "internal/httpapi/cookies_test.go";

fn boot(width: u16, height: u16) -> AppState {
    let mut s = AppState::new(
        Theme::dark(),
        Boot {
            session_id: SessionId::new("perf"),
            user: "tester".into(),
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
    s.size = (width, height);
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m0"),
        }),
    );
    s
}

fn content(lines: usize) -> String {
    (1..=lines)
        .map(|i| format!("    expectCookie{i}(t)\n"))
        .collect()
}

/// A real applied diff for a created file of `lines` rows, built outside any
/// measured region.
fn created_diff(lines: usize) -> String {
    let body = content(lines);
    let hunks = leveler_tools::tools::applied_diff::whole_file_hunks("", &body);
    leveler_tools::tools::applied_diff::unified_diff(PATH, &hunks).expect("real applied diff")
}

/// A minimal unified diff (3 lines of context) — the shape a patch tool
/// reports for an edit inside a large file, unlike a whole-file write.
fn minimal_hunk(
    start: u32,
    old: &[&str],
    new: &[&str],
    context_before: &[&str],
    context_after: &[&str],
) -> String {
    let mut text = String::from("--- a/");
    text.push_str(PATH);
    text.push_str("\n+++ b/");
    text.push_str(PATH);
    text.push('\n');
    let old_len = old.len() + context_before.len() + context_after.len();
    let new_len = new.len() + context_before.len() + context_after.len();
    text.push_str(&format!("@@ -{start},{old_len} +{start},{new_len} @@\n"));
    for line in context_before {
        text.push(' ');
        text.push_str(line);
        text.push('\n');
    }
    for line in old {
        text.push('-');
        text.push_str(line);
        text.push('\n');
    }
    for line in new {
        text.push('+');
        text.push_str(line);
        text.push('\n');
    }
    for line in context_after {
        text.push(' ');
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// Several far-apart hunks of one file: the applied diff a patch tool reports
/// for a multi-site change.
fn multi_hunk_diff(hunks: usize) -> String {
    let mut text = String::from("--- a/");
    text.push_str(PATH);
    text.push_str("\n+++ b/");
    text.push_str(PATH);
    text.push('\n');
    for k in 0..hunks {
        let start = 100 + (k as u32) * 1_000;
        text.push_str(&format!("@@ -{start},5 +{start},6 @@\n"));
        text.push_str("     fn untouched_before() {}\n");
        text.push_str("     fn untouched_after() {}\n");
        text.push_str(&format!("-    let value{k} = old_{k};\n"));
        text.push_str(&format!("+    let value{k} = new_{k};\n"));
        text.push_str(&format!("+    check_{k}(value{k});\n"));
        text.push_str("     // still here\n");
    }
    text
}

/// A diff whose lines are far wider than any terminal.
fn wide_line_diff(lines: usize, width: usize) -> String {
    let mut text = String::from("--- a/");
    text.push_str(PATH);
    text.push_str("\n+++ b/");
    text.push_str(PATH);
    text.push_str(&format!("\n@@ -1,{lines} +1,{lines} @@\n"));
    for i in 0..lines {
        let payload = "x".repeat(width);
        text.push_str(&format!("+    let wide_{i} = \"{payload}\";\n"));
    }
    text
}

fn install_edit(s: &mut AppState, diff: String) {
    let id = ToolCallId::new("edit");
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: id.clone(),
            name: "write_file".into(),
            arguments: serde_json::json!({ "path": PATH, "content": "" }).to_string(),
            parallel: false,
            model_step: Some(0),
            answer_effect: None,
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id,
            ok: true,
            preview: format!("Created {PATH}"),
            duration_ms: 4,
            applied_diff: Some(diff),
            exit_code: None,
            stop: None,
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: MessageId::new("m1"),
        }),
    );
}

fn timed(mut f: impl FnMut()) -> Duration {
    let start = Instant::now();
    f();
    start.elapsed()
}

fn peak_during<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let before_live = LIVE.load(Ordering::Relaxed);
    PEAK.store(before_live, Ordering::Relaxed);
    let out = f();
    let peak = PEAK.load(Ordering::Relaxed);
    (out, peak.saturating_sub(before_live))
}

fn frame(s: &mut AppState, w: u16, h: u16) -> usize {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| render(f, s)).unwrap();
    term.backend().buffer().content.len()
}

struct Row {
    label: &'static str,
    rows: usize,
    cold: Duration,
    warm: Duration,
    fold: Duration,
    unfold: Duration,
    resize: Duration,
    scroll: Duration,
    peak_bytes: usize,
    frame_lines: usize,
}

fn measure(shape: &Shape, width: u16) -> Row {
    let mut s = boot(width, 60);
    install_edit(&mut s, shape.diff.clone());
    // Cold: the first projection of this transcript at this width, with an
    // empty item cache. The patch text is already built (outside the timer), and
    // peak heap is sampled around the same region so the two numbers describe
    // one build.
    let mut frame_lines = 0usize;
    let mut cold = Duration::ZERO;
    let (_, peak_bytes) = peak_during(|| {
        cold = timed(|| {
            frame_lines = conversation_line_count(&s, usize::from(width));
        });
    });
    // Warm: the memoized path every later frame uses.
    let warm = timed(|| {
        let _ = conversation_line_count(&s, usize::from(width));
    });
    // Fold and unfold through the real key path (Ctrl+O targets the edit).
    let fold = timed(|| {
        reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
        );
        let _ = conversation_line_count(&s, usize::from(width));
    });
    let unfold = timed(|| {
        reduce(
            &mut s,
            Action::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
        );
        let _ = conversation_line_count(&s, usize::from(width));
    });
    // Resize re-wraps at another width, then back — the layout a real terminal
    // pays on every window drag.
    let resize = timed(|| {
        let _ = conversation_line_count(&s, 40);
        let _ = conversation_line_count(&s, usize::from(width));
    });
    // A frame's FIRST paint after a projection change pays for the memo entry
    // at the viewport's own content width (a few columns narrower than the
    // terminal), which `cold` above measures. Scrolling and resize bursts then
    // reuse it, so the steady-state frame is what this column reports.
    frame(&mut s, width, 60);
    const FRAMES: u32 = 20;
    let scroll = timed(|| {
        for _ in 0..FRAMES {
            frame(&mut s, width, 60);
        }
    }) / FRAMES;
    Row {
        label: shape.label,
        rows: shape.source_rows,
        cold,
        warm,
        fold,
        unfold,
        resize,
        scroll,
        peak_bytes,
        frame_lines,
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// One measured shape.
struct Shape {
    label: &'static str,
    source_rows: usize,
    diff: String,
}

fn shapes() -> Vec<Shape> {
    let mut out = Vec::new();
    for (lines, label) in [
        (100usize, "created 100"),
        (1_000, "created 1k"),
        (5_000, "created 5k"),
        (10_000, "created 10k"),
        (100_000, "created 100k"),
    ] {
        out.push(Shape {
            label,
            source_rows: lines,
            diff: created_diff(lines),
        });
    }
    // A one-line change inside a large file: a real minimal hunk, not a
    // whole-file write.
    out.push(Shape {
        label: "1 small hunk",
        source_rows: 1,
        diff: minimal_hunk(
            5_000,
            &["    let value = old;"],
            &["    let value = new;"],
            &["    fn context_before() {}", "    // nearby"],
            &["    fn context_after() {}", "    // nearby"],
        ),
    });
    // Six far-apart hunks of one file.
    out.push(Shape {
        label: "6 disjoint hunks",
        source_rows: 6,
        diff: multi_hunk_diff(6),
    });
    // Long lines: the soft-wrap worst case.
    out.push(Shape {
        label: "400 wide lines",
        source_rows: 400,
        diff: wide_line_diff(400, 600),
    });
    out
}

#[test]
fn diff_perf_matrix() {
    let shapes = shapes();
    // One-off initialization (syntax definitions, theme tables, the static
    // string tables) belongs to neither shape; pay it before measuring.
    {
        let mut warm = boot(120, 60);
        install_edit(&mut warm, created_diff(3));
        let _ = conversation_line_count(&warm, 120);
        let _ = frame(&mut warm, 120, 60);
    }
    let mut rows = Vec::new();
    for shape in &shapes {
        rows.push(measure(shape, 120));
    }

    println!(
        "\n{:<17} {:>7} {:>9} {:>9} {:>9} {:>9} {:>10} {:>10} {:>9} {:>8}",
        "shape",
        "rows",
        "cold_ms",
        "warm_ms",
        "fold_ms",
        "open_ms",
        "resize_ms",
        "frame_ms",
        "peak_MiB",
        "lines"
    );
    for r in &rows {
        println!(
            "{:<17} {:>7} {:>9.2} {:>9.3} {:>9.2} {:>9.2} {:>10.2} {:>10.2} {:>9.1} {:>8}",
            r.label,
            r.rows,
            ms(r.cold),
            ms(r.warm),
            ms(r.fold),
            ms(r.unfold),
            ms(r.resize),
            ms(r.scroll),
            r.peak_bytes as f64 / (1024.0 * 1024.0),
            r.frame_lines
        );
    }

    // Second pass over the same shapes in the same process: a cost that is
    // still there is per-shape work, not one-time initialization.
    let mut second = Vec::new();
    for shape in &shapes {
        second.push(measure(shape, 120));
    }
    println!(
        "\n{:<17} {:>9} {:>9}",
        "shape (2nd pass)", "cold_ms", "open_ms"
    );
    for r in &second {
        println!("{:<17} {:>9.2} {:>9.2}", r.label, ms(r.cold), ms(r.unfold));
    }

    // Content completeness: the projection paints every source row it was
    // given, plus chrome. Nothing is elided by a line-count heuristic.
    for r in &rows {
        assert!(
            r.frame_lines > r.rows,
            "{}: only {} conversation lines for {} source rows",
            r.label,
            r.frame_lines,
            r.rows
        );
    }
    // Warm frames must not re-wrap the world: the memoized path is O(1) per
    // frame in every size.
    for r in &rows {
        assert!(
            ms(r.warm) < ms(r.cold).max(1.0),
            "{}: warm {:.3} ms vs cold {:.2} ms — the memo is not being reused",
            r.label,
            ms(r.warm),
            ms(r.cold)
        );
    }
    // Guard rails, not targets: several times the observed numbers, so a real
    // regression (an O(n²) pass, a lost cache, a re-highlight per frame) fails
    // loudly while machine noise does not.
    let max = rows
        .iter()
        .find(|r| r.rows == 100_000)
        .expect("the 100k shape");
    assert!(
        ms(max.cold) < 20_000.0,
        "{} cold build took {:.0} ms",
        max.label,
        ms(max.cold)
    );
    assert!(
        ms(max.warm) < 25.0,
        "{} warm build took {:.2} ms — every frame pays the full projection",
        max.label,
        ms(max.warm)
    );
    assert!(
        ms(max.scroll) < 250.0,
        "{} frame took {:.0} ms",
        max.label,
        ms(max.scroll)
    );
    // A small hunk must not cost like the file it lives in: its own projection
    // is bounded by its own rows.
    let small = rows
        .iter()
        .find(|r| r.label == "1 small hunk")
        .expect("the small-hunk shape");
    assert!(
        small.frame_lines < 40,
        "a one-line change painted {} rows",
        small.frame_lines
    );
}

/// 24-column frames are the worst case for soft wrapping: every source row
/// becomes several painted rows. The projection must stay usable there too.
#[test]
fn diff_perf_narrow_frame() {
    for lines in [1_000usize, 10_000] {
        let mut s = boot(24, 40);
        install_edit(&mut s, created_diff(lines));
        let (painted, peak) = peak_during(|| conversation_line_count(&s, 24));
        let warm = timed(|| {
            let _ = conversation_line_count(&s, 24);
        });
        let repaint = timed(|| {
            for _ in 0..10 {
                frame(&mut s, 24, 40);
            }
        }) / 10;
        println!(
            "narrow 24x40 {} rows: painted {} lines, warm {:.3} ms, frame {:.2} ms, peak {:.1} MiB",
            lines,
            painted,
            ms(warm),
            ms(repaint),
            peak as f64 / (1024.0 * 1024.0)
        );
        assert!(
            painted > lines,
            "wrapping must add rows, not remove the diff: {painted} for {lines}"
        );
        assert!(
            ms(repaint) < 250.0,
            "a 24-column frame of {lines} rows took {:.0} ms",
            ms(repaint)
        );
    }
}
