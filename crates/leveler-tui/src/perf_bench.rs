//! Deterministic streaming/render benchmark harness (ignored tests).
//!
//! Run with:
//! ```text
//! cargo test -p leveler-tui --lib perf_bench -- --ignored --nocapture
//! ```
//!
//! These tests exercise the *real* reducer and the *real* renderer, with no
//! provider, network, model, or PTY in the loop. The point is reproducibility:
//! the same fixture always produces the same curve, so a change to the render
//! pipeline can be compared against a fixed baseline.
//!
//! Rendering goes to a Ratatui `TestBackend`. That measures everything above
//! the terminal (projection, markdown, wrapping, layout, buffer diff) but not
//! the OS write; the profile harness measures the real `terminal.draw` on a PTY
//! separately.

use std::time::Instant;

use leveler_client_protocol::{
    MessageId, PermissionProfile, RuntimeEvent, SessionId, ToolCallId, UiMessage, UiRole,
    UiSessionSnapshot,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::action::Action;
use crate::reducer::reduce;
use crate::state::{AppState, Boot};
use crate::theme::Theme;

const WIDTH: usize = 120;
const HEIGHT: u16 = 40;
/// The busy repaint cadence from `run.rs`. Replays pace frames with it so the
/// amplification numbers reflect the actual scheduling policy, not a guess.
const BUSY_TICK_US: u64 = 150_000;

fn opened() -> AppState {
    let mut state = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("perf"),
            user: "perf".into(),
            version: "0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: crate::i18n::Locale::Zh,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    );
    state.size = (WIDTH as u16, HEIGHT);
    let snap = UiSessionSnapshot {
        id: SessionId::new("perf"),
        repository: "~/perf".into(),
        goal: "benchmark".into(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
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
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    state
}

fn runtime(state: &mut AppState, event: RuntimeEvent) {
    reduce(state, Action::Runtime(event));
}

/// Deterministic Markdown of about `target` bytes: headings, prose, lists,
/// fenced code, a table, and a quote — the block shapes that cost the most.
fn long_markdown(target: usize) -> String {
    let mut out = String::with_capacity(target + 256);
    let mut i = 0usize;
    while out.len() < target {
        out.push_str(&format!(
            "## Section {i}\n\n\
             Paragraph {i} with **bold**, *italic*, `inline code`, and a [link](https://example.com/{i}).\n\n\
             - item one {i}\n- item two {i}\n- item three {i}\n\n\
             ```rust\nfn example_{i}() {{\n    println!(\"hello {i}\");\n}}\n```\n\n\
             | col a | col b |\n|---|---|\n| {i} | {i} |\n\n\
             > quoted line {i}\n\n"
        ));
        i += 1;
    }
    out.truncate(target);
    out
}

fn plain_text(target: usize) -> String {
    let mut out = String::with_capacity(target + 64);
    let mut i = 0usize;
    while out.len() < target {
        out.push_str(&format!(
            "Chunk {i}: the quick brown fox jumps over the lazy dog while streaming text accumulates. "
        ));
        i += 1;
    }
    out.truncate(target);
    out
}

/// Same block mix as [`long_markdown`] but WITHOUT fenced code, to isolate the
/// syntect syntax-highlighting cost from pulldown-cmark parsing.
fn long_markdown_no_code(target: usize) -> String {
    let mut out = String::with_capacity(target + 256);
    let mut i = 0usize;
    while out.len() < target {
        out.push_str(&format!(
            "## Section {i}\n\n\
             Paragraph {i} with **bold**, *italic*, `inline code`, and a [link](https://example.com/{i}).\n\n\
             - item one {i}\n- item two {i}\n- item three {i}\n\n\
             | col a | col b |\n|---|---|\n| {i} | {i} |\n\n\
             > quoted line {i}\n\n"
        ));
        i += 1;
    }
    out.truncate(target);
    out
}

/// One giant fenced block of `target` bytes of Rust, the streaming-code case
/// where the tail block keeps changing and only a warm re-parse of the same
/// content can hit the highlight memo.
fn one_giant_fence(target: usize) -> String {
    let mut out = String::from("```rust\n");
    let mut i = 0usize;
    while out.len() < target {
        out.push_str(&format!(
            "fn giant_{i}(x: i64) -> i64 {{ x * {i} + {i} }}\n"
        ));
        i += 1;
    }
    out.push_str("```\n");
    out
}

fn ui_user(i: usize) -> UiMessage {
    UiMessage {
        id: MessageId::new(format!("u{i}")),
        role: UiRole::User,
        text: format!("turn {i}: do the thing"),
        ordinal: None,
        kind: None,
        images: 0,
    }
}

/// Finalize one realistic assistant turn (streamed then completed) and one
/// small tool group, so history contains exactly what a long session does.
fn push_history(state: &mut AppState, turns: usize, bytes_per_message: usize) {
    for i in 0..turns {
        runtime(
            state,
            RuntimeEvent::UserMessageAdded {
                message: ui_user(i),
            },
        );
        let mid = MessageId::new(format!("a{i}"));
        runtime(
            state,
            RuntimeEvent::AssistantMessageStarted {
                message_id: mid.clone(),
            },
        );
        let text = long_markdown(bytes_per_message);
        for chunk in text.as_bytes().chunks(128) {
            runtime(
                state,
                RuntimeEvent::AssistantTextDelta {
                    message_id: mid.clone(),
                    delta: String::from_utf8_lossy(chunk).into_owned(),
                },
            );
        }
        runtime(
            state,
            RuntimeEvent::AssistantMessageCompleted {
                message_id: mid.clone(),
            },
        );
        let call = ToolCallId::new(format!("tc{i}"));
        runtime(
            state,
            RuntimeEvent::ToolCallStarted {
                id: call.clone(),
                name: "read_file".into(),
                arguments: format!("{{\"path\":\"src/file_{i}.rs\"}}"),
                parallel: false,
            },
        );
        runtime(
            state,
            RuntimeEvent::ToolCallCompleted {
                id: call,
                ok: true,
                preview: format!("{i}: ok"),
                duration_ms: 12,
                applied_diff: None,
                exit_code: None,
                stop: None,
            },
        );
    }
}

/// Start a streaming assistant message and append `text` in 128-byte deltas.
fn stream_assistant(state: &mut AppState, id: &str, text: &str) {
    let mid = MessageId::new(id);
    runtime(
        state,
        RuntimeEvent::AssistantMessageStarted {
            message_id: mid.clone(),
        },
    );
    for chunk in text.as_bytes().chunks(128) {
        runtime(
            state,
            RuntimeEvent::AssistantTextDelta {
                message_id: mid.clone(),
                delta: String::from_utf8_lossy(chunk).into_owned(),
            },
        );
    }
}

/// Force a line-cache miss so the next build really runs. An empty delta bumps
/// the transcript version without changing any content.
fn invalidate(state: &mut AppState, id: &str) {
    runtime(
        state,
        RuntimeEvent::AssistantTextDelta {
            message_id: MessageId::new(id),
            delta: String::new(),
        },
    );
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn pct(mut v: Vec<f64>, q: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
    v[idx.min(v.len() - 1)]
}

struct Row {
    label: String,
    bytes: usize,
    lines: usize,
    parse_ms: f64,
    wrap_ms: f64,
    build_ms: f64,
    draw_ms: f64,
}

fn measure(state: &mut AppState, id: &str, label: String, bytes: usize) -> Row {
    let width = WIDTH;
    let theme = state.theme.clone();
    let text = match state.transcript.items().last() {
        Some(crate::transcript::TranscriptItem::Assistant(block)) => block.text.clone(),
        _ => String::new(),
    };
    // Markdown parse cost of this exact live message.
    let mut parse = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let _ = crate::markdown::MdDoc::parse(&text);
        parse.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    // Layout / wrapping cost of the parsed doc.
    let doc = crate::markdown::MdDoc::parse(&text);
    let mut wrap = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let _ = doc.to_lines(width, &theme);
        wrap.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    // Full conversation build (all items + live message), cache miss.
    let mut build = Vec::new();
    for _ in 0..15 {
        invalidate(state, id);
        let t = Instant::now();
        let _ = state.conversation_build(width);
        build.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let lines = state.conversation_lines(width).len();
    // Full frame into a TestBackend, cache miss (so projection is included).
    let mut term = Terminal::new(TestBackend::new(WIDTH as u16, HEIGHT)).unwrap();
    let mut draw = Vec::new();
    for _ in 0..15 {
        invalidate(state, id);
        let t = Instant::now();
        term.draw(|f| crate::render::render(f, state)).unwrap();
        draw.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    Row {
        label,
        bytes,
        lines,
        parse_ms: median(parse),
        wrap_ms: median(wrap),
        build_ms: median(build),
        draw_ms: median(draw),
    }
}

fn print_rows(title: &str, rows: &[Row]) {
    println!("\n=== {title} ===");
    println!(
        "{:<12} {:>9} {:>8} {:>10} {:>10} {:>11} {:>11}",
        "label", "bytes", "lines", "parse_ms", "wrap_ms", "build_ms", "draw_ms"
    );
    for r in rows {
        println!(
            "{:<12} {:>9} {:>8} {:>10.3} {:>10.3} {:>11.3} {:>11.3}",
            r.label, r.bytes, r.lines, r.parse_ms, r.wrap_ms, r.build_ms, r.draw_ms
        );
    }
}

/// §7 message-size degradation: one live streaming message, tiny fixed history.
#[test]
#[ignore = "manual performance harness; run with --ignored --nocapture"]
fn perf_message_size_curve() {
    let sizes = [1_000usize, 5_000, 10_000, 25_000, 50_000, 100_000];
    let mut rows = Vec::new();
    for size in sizes {
        let mut state = opened();
        push_history(&mut state, 1, 600);
        stream_assistant(&mut state, "live", &long_markdown(size));
        rows.push(measure(&mut state, "live", format!("{size}"), size));
    }
    print_rows("message-size curve (long markdown, 1 history turn)", &rows);
}

/// §9 highlight-memo cold/warm split for the three fenced-code shapes.
/// `multi-code` is many small closed fences, `one-giant` is a single growing
/// fence, `no-code` is the no-syntect control. Cold clears the memo first;
/// warm re-parses the identical bytes five times.
#[test]
#[ignore = "manual performance harness; run with --ignored --nocapture"]
fn perf_markdown_cache_curve() {
    let sizes = [10_000usize, 50_000, 100_000];
    println!("\n=== markdown highlight cache (cold vs warm) ===");
    println!(
        "{:<12} {:>8} {:>10} {:>10} {:>8} {:>8} {:>9}",
        "fixture", "bytes", "cold_ms", "warm_ms", "hits", "misses", "speedup"
    );
    let fixtures: [(&str, fn(usize) -> String); 3] = [
        ("multi-code", long_markdown),
        ("one-giant", one_giant_fence),
        ("no-code", long_markdown_no_code),
    ];
    for (label, make) in fixtures {
        for size in sizes {
            let text = make(size);
            crate::markdown::highlight_cache_clear();
            let (h0, m0) = crate::markdown::highlight_cache_stats();
            let t = Instant::now();
            let _ = crate::markdown::MdDoc::parse(&text);
            let cold = t.elapsed().as_secs_f64() * 1000.0;
            let mut warm = Vec::new();
            for _ in 0..5 {
                let t = Instant::now();
                let _ = crate::markdown::MdDoc::parse(&text);
                warm.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            let warm = median(warm);
            let (h1, m1) = crate::markdown::highlight_cache_stats();
            println!(
                "{:<12} {:>8} {:>10.2} {:>10.2} {:>8} {:>8} {:>8.1}x",
                label,
                text.len(),
                cold,
                warm,
                h1 - h0,
                m1 - m0,
                cold / warm.max(0.001),
            );
        }
    }
}

/// §7b pure-text variant, to separate Markdown cost from raw wrapping.
#[test]
#[ignore = "manual performance harness; run with --ignored --nocapture"]
fn perf_message_size_curve_plain() {
    let sizes = [1_000usize, 5_000, 10_000, 25_000, 50_000, 100_000];
    let mut rows = Vec::new();
    for size in sizes {
        let mut state = opened();
        push_history(&mut state, 1, 600);
        stream_assistant(&mut state, "live", &plain_text(size));
        rows.push(measure(&mut state, "live", format!("{size}"), size));
    }
    print_rows("message-size curve (plain text)", &rows);
}

/// §10 H4 attribution: markdown with the same structure but no fenced code.
/// The delta against `perf_message_size_curve` is the syntect cost.
#[test]
#[ignore = "manual performance harness; run with --ignored --nocapture"]
fn perf_message_size_curve_no_code() {
    let sizes = [1_000usize, 5_000, 10_000, 25_000, 50_000, 100_000];
    let mut rows = Vec::new();
    for size in sizes {
        let mut state = opened();
        push_history(&mut state, 1, 600);
        stream_assistant(&mut state, "live", &long_markdown_no_code(size));
        rows.push(measure(&mut state, "live", format!("{size}"), size));
    }
    print_rows("message-size curve (markdown, no code fences)", &rows);
}

/// §11 tool/activity projection: does a growing tool group rebuild every frame
/// at a cost that stays linear?
#[test]
#[ignore = "manual performance harness; run with --ignored --nocapture"]
fn perf_tool_stream_curve() {
    let counts = [0usize, 20, 50, 100, 200];
    let mut rows = Vec::new();
    for n in counts {
        let mut state = opened();
        push_history(&mut state, 1, 600);
        for i in 0..n {
            let call = ToolCallId::new(format!("tc{i}"));
            runtime(
                &mut state,
                RuntimeEvent::ToolCallStarted {
                    id: call.clone(),
                    name: "read_file".into(),
                    arguments: format!("{{\"path\":\"src/module_{i}.rs\"}}"),
                    parallel: false,
                },
            );
            runtime(
                &mut state,
                RuntimeEvent::ToolCallCompleted {
                    id: call,
                    ok: true,
                    preview: format!("{i}: ok"),
                    duration_ms: 5,
                    applied_diff: None,
                    exit_code: None,
                    stop: None,
                },
            );
        }
        stream_assistant(&mut state, "live", &long_markdown(2_000));
        rows.push(measure(&mut state, "live", format!("{n} tools"), n));
    }
    print_rows("tool-count curve (2 KB live message)", &rows);
}

/// §8/§12 the decisive one: the real event loop calls `sync_scroll` every
/// iteration, and that rebuilds the conversation whenever the transcript
/// version changed — i.e. on EVERY delta, not once per painted frame. This
/// models that call directly and measures the per-delta rebuild cost.
#[test]
#[ignore = "manual performance harness; run with --ignored --nocapture"]
fn perf_per_delta_rebuild() {
    println!("\n=== per-delta conversation rebuild (sync_scroll) ===");
    println!(
        "{:<10} {:>8} {:>10} {:>12} {:>12} {:>12}",
        "size", "deltas", "total_ms", "rebuild_p50", "rebuild_p95", "rebuild_max"
    );
    for size in [2_000usize, 10_000, 25_000] {
        let mut state = opened();
        push_history(&mut state, 1, 600);
        let text = long_markdown(size);
        let mid = MessageId::new("live");
        runtime(
            &mut state,
            RuntimeEvent::AssistantMessageStarted {
                message_id: mid.clone(),
            },
        );
        let mut rebuild = Vec::new();
        let mut total_ms = 0.0f64;
        let mut deltas = 0u64;
        for chunk in text.as_bytes().chunks(64) {
            runtime(
                &mut state,
                RuntimeEvent::AssistantTextDelta {
                    message_id: mid.clone(),
                    delta: String::from_utf8_lossy(chunk).into_owned(),
                },
            );
            let t = Instant::now();
            crate::conversation::sync_scroll(&mut state);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            total_ms += ms;
            rebuild.push(ms);
            deltas += 1;
        }
        println!(
            "{:<10} {:>8} {:>10.0} {:>12.3} {:>12.3} {:>12.3}",
            size,
            deltas,
            total_ms,
            pct(rebuild.clone(), 0.50),
            pct(rebuild.clone(), 0.95),
            rebuild.iter().cloned().fold(0.0, f64::max),
        );
    }
    println!(
        "(a stream that arrives in T seconds spends this much CPU before it can paint;\n \
         a per-frame budget of 150 ms cannot absorb rebuild_p95 above 150 ms)"
    );
}

/// §8 history-size degradation: fixed live message, growing finalized history.
#[test]
#[ignore = "manual performance harness; run with --ignored --nocapture"]
fn perf_history_size_curve() {
    let turns = [1usize, 10, 25, 50, 100, 200];
    let live = long_markdown(5_000);
    let mut rows = Vec::new();
    for n in turns {
        let mut state = opened();
        push_history(&mut state, n, 2_000);
        stream_assistant(&mut state, "live", &live);
        let history_bytes_snapshot = state
            .transcript
            .items()
            .iter()
            .take(state.transcript.items().len().saturating_sub(1))
            .filter_map(|item| match item {
                crate::transcript::TranscriptItem::Assistant(b) => Some(b.text.len()),
                _ => None,
            })
            .sum::<usize>();
        rows.push(measure(
            &mut state,
            "live",
            format!("{n} turns"),
            history_bytes_snapshot,
        ));
    }
    print_rows("history-size curve (5 KB live message)", &rows);
}

/// §28 the fix's A/B: the same fixture replayed with the projection rebuilt
/// once per delta (before) vs once per painted frame (after). Identical input;
/// only the sync timing differs.
#[test]
#[ignore = "manual performance harness; run with --ignored --nocapture"]
fn perf_sync_coalescing_ab() {
    let _ = crate::markdown::MdDoc::parse(&long_markdown(64));
    println!("\n=== sync coalescing A/B (same fixture) ===");
    println!(
        "{:<16} {:<10} {:>7} {:>12} {:>12} {:>12}",
        "fixture", "mode", "frames", "rebuild_ms", "paint_ms", "total_ms"
    );
    for (label, deltas, interval) in [("6KB/150d", 150usize, 8_000u64), ("13KB/400d", 400, 8_000)] {
        let events = synth_markdown(deltas, interval);
        for mode in [SyncMode::PerDelta, SyncMode::PerFrame] {
            let (frames, rebuild, paint, lat) = simulate_sync(&events, mode);
            println!(
                "{:<16} {:<10} {:>7} {:>12.0} {:>12.0} {:>12.0}",
                label,
                match mode {
                    SyncMode::PerDelta => "per-delta",
                    SyncMode::PerFrame => "per-frame",
                },
                frames,
                rebuild,
                paint,
                rebuild + paint,
            );
            if !lat.is_empty() {
                println!(
                    "{:<16} {:<10} {:>7} {:>12.1} {:>12.1} {:>12.1}",
                    "",
                    "event→frame p50/p95/max",
                    "",
                    pct(lat.clone(), 0.50),
                    pct(lat.clone(), 0.95),
                    lat.iter().cloned().fold(0.0, f64::max),
                );
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SyncMode {
    PerDelta,
    PerFrame,
}

/// Replay `events` against the real reducer + renderer. `PerDelta` mirrors the
/// pre-fix loop (sync rebuilds on every iteration); `PerFrame` mirrors the fix
/// (sync rebuilds only when a frame is painted). Returns
/// `(frames, rebuild_ms, paint_ms, event_to_frame_end_ms)`.
fn simulate_sync(
    events: &[crate::record::RecordedEvent],
    mode: SyncMode,
) -> (u64, f64, f64, Vec<f64>) {
    let mut state = opened();
    let mut term = Terminal::new(TestBackend::new(WIDTH as u16, HEIGHT)).unwrap();
    let mut next_frame = 0u64;
    let mut frames = 0u64;
    let mut rebuild_ms = 0.0f64;
    let mut paint_ms = 0.0f64;
    let mut pending: Vec<u64> = Vec::new();
    let mut latencies: Vec<f64> = Vec::new();
    for row in events {
        while next_frame <= row.offset_us {
            // Time an event arriving before this frame waits for it: the rest
            // of the tick plus the rebuild the frame performs before it paints.
            let frame_start = next_frame;
            if mode == SyncMode::PerFrame {
                let t = Instant::now();
                crate::conversation::sync_scroll(&mut state);
                let rebuild = t.elapsed().as_secs_f64() * 1000.0;
                rebuild_ms += rebuild;
                for offset in pending.drain(..) {
                    let wait = (frame_start.saturating_sub(offset)) as f64 / 1000.0;
                    latencies.push(wait + rebuild);
                }
            }
            let t = Instant::now();
            term.draw(|f| crate::render::render(f, &mut state)).unwrap();
            paint_ms += t.elapsed().as_secs_f64() * 1000.0;
            frames += 1;
            next_frame += BUSY_TICK_US;
        }
        if mode == SyncMode::PerDelta {
            let t = Instant::now();
            crate::conversation::sync_scroll(&mut state);
            rebuild_ms += t.elapsed().as_secs_f64() * 1000.0;
        }
        if matches!(row.event, RuntimeEvent::AssistantTextDelta { .. }) {
            pending.push(row.offset_us);
        }
        runtime(&mut state, row.event.clone());
    }
    (frames, rebuild_ms, paint_ms, latencies)
}

/// §16 deterministic replay of the four fixtures, paced by the real 150 ms
/// busy-tick policy, reporting render amplification and per-event apply cost.
#[test]
#[ignore = "manual performance harness; run with --ignored --nocapture"]
fn perf_replay_fixtures() {
    // Warm the lazy syntect/SyntaxSet init so it does not show up as a spike.
    let _ = crate::markdown::MdDoc::parse(&long_markdown(64));
    for (name, events) in fixtures() {
        let mut state = opened();
        let mut frames = 0u64;
        let mut next_frame = 0u64;
        let mut apply = Vec::new();
        let mut last_text_len = 0usize;
        let mut deltas = 0u64;
        let mut deltas_since_frame = 0u64;
        let mut deltas_per_frame: Vec<f64> = Vec::new();
        let mut draw = Vec::new();
        let mut term = Terminal::new(TestBackend::new(WIDTH as u16, HEIGHT)).unwrap();

        for row in &events {
            // Frames happen on the busy tick while a turn is streaming. Model
            // the policy directly: one frame per 150 ms of wall time.
            while next_frame <= row.offset_us {
                deltas_per_frame.push(deltas_since_frame as f64);
                deltas_since_frame = 0;
                let t = Instant::now();
                term.draw(|f| crate::render::render(f, &mut state)).unwrap();
                draw.push(t.elapsed().as_secs_f64() * 1000.0);
                frames += 1;
                next_frame += BUSY_TICK_US;
            }
            if let RuntimeEvent::AssistantTextDelta { delta, .. } = &row.event {
                deltas += 1;
                deltas_since_frame += 1;
                last_text_len += delta.len();
            }
            let t = Instant::now();
            runtime(&mut state, row.event.clone());
            apply.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let duration_s = events.last().map(|r| r.offset_us).unwrap_or(0) as f64 / 1e6;
        println!("\n=== replay fixture: {name} ===");
        println!(
            "events={} deltas={} duration_s={:.2} frames={} render/delta={:.3} delta/render={:.2}",
            events.len(),
            deltas,
            duration_s,
            frames,
            frames as f64 / deltas.max(1) as f64,
            deltas as f64 / frames.max(1) as f64,
        );
        println!(
            "apply_ms p50={:.4} p95={:.4} max={:.4} | draw_ms p50={:.3} p95={:.3} max={:.3} | deltas/frame p50={:.1} p95={:.1} max={:.1} | streamed_bytes={}",
            pct(apply.clone(), 0.50),
            pct(apply.clone(), 0.95),
            apply.iter().cloned().fold(0.0, f64::max),
            pct(draw.clone(), 0.50),
            pct(draw.clone(), 0.95),
            draw.iter().cloned().fold(0.0, f64::max),
            pct(deltas_per_frame.clone(), 0.50),
            pct(deltas_per_frame.clone(), 0.95),
            deltas_per_frame.iter().cloned().fold(0.0, f64::max),
            last_text_len,
        );
        // Deterministic round-trip through the real recorder format.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.jsonl");
        crate::record::write_stream(&path, &events).unwrap();
        let read = crate::record::read_stream(&path).unwrap();
        assert_eq!(
            read.len(),
            events.len(),
            "fixture round-trip must be lossless"
        );
    }
}

/// Fixtures A–D and a mixed tool stream, with realistic relative timing.
fn fixtures() -> Vec<(&'static str, Vec<crate::record::RecordedEvent>)> {
    vec![
        // A: ~1k deltas, pure long text.
        ("A_plain_1k", synth_plain(1_000, 15_000)),
        // B: ~5k deltas, long markdown.
        ("B_markdown_5k", synth_markdown(5_000, 8_000)),
        // C: mixed reasoning / tool / text / final.
        ("C_mixed_tools", synth_mixed()),
        // D: long history then streaming.
        ("D_long_history", synth_long_history(60)),
    ]
}

fn ev(seq: u64, offset_us: u64, event: RuntimeEvent) -> crate::record::RecordedEvent {
    crate::record::RecordedEvent {
        seq,
        offset_us,
        event,
    }
}

fn synth_plain(deltas: usize, interval_us: u64) -> Vec<crate::record::RecordedEvent> {
    let mut out = Vec::new();
    let mid = MessageId::new("live");
    let mut seq = 0u64;
    let mut t = 0u64;
    out.push(ev(
        seq,
        t,
        RuntimeEvent::AssistantMessageStarted {
            message_id: mid.clone(),
        },
    ));
    seq += 1;
    let payload = plain_text(deltas * 24);
    for chunk in payload.as_bytes().chunks(24).take(deltas) {
        t += interval_us;
        out.push(ev(
            seq,
            t,
            RuntimeEvent::AssistantTextDelta {
                message_id: mid.clone(),
                delta: String::from_utf8_lossy(chunk).into_owned(),
            },
        ));
        seq += 1;
    }
    out.push(ev(
        seq,
        t + interval_us,
        RuntimeEvent::AssistantMessageCompleted {
            message_id: mid.clone(),
        },
    ));
    out
}

fn synth_markdown(deltas: usize, interval_us: u64) -> Vec<crate::record::RecordedEvent> {
    let mut out = Vec::new();
    let mid = MessageId::new("live");
    let mut seq = 0u64;
    let mut t = 0u64;
    out.push(ev(
        seq,
        t,
        RuntimeEvent::AssistantMessageStarted {
            message_id: mid.clone(),
        },
    ));
    seq += 1;
    let payload = long_markdown(deltas * 32);
    for chunk in payload.as_bytes().chunks(32).take(deltas) {
        t += interval_us;
        out.push(ev(
            seq,
            t,
            RuntimeEvent::AssistantTextDelta {
                message_id: mid.clone(),
                delta: String::from_utf8_lossy(chunk).into_owned(),
            },
        ));
        seq += 1;
    }
    out.push(ev(
        seq,
        t + interval_us,
        RuntimeEvent::AssistantMessageCompleted {
            message_id: mid.clone(),
        },
    ));
    out
}

fn synth_mixed() -> Vec<crate::record::RecordedEvent> {
    let mut out = Vec::new();
    let mut seq = 0u64;
    let mut t = 0u64;
    let step = 60_000u64;
    let mid = MessageId::new("live");
    out.push(ev(
        seq,
        t,
        RuntimeEvent::AssistantMessageStarted {
            message_id: mid.clone(),
        },
    ));
    seq += 1;
    for i in 0..40 {
        t += step;
        out.push(ev(
            seq,
            t,
            RuntimeEvent::ReasoningDelta {
                delta: format!("thinking step {i} "),
            },
        ));
        seq += 1;
        if i % 8 == 0 {
            let call = ToolCallId::new(format!("tc{i}"));
            out.push(ev(
                seq,
                t,
                RuntimeEvent::ToolCallStarted {
                    id: call.clone(),
                    name: "bash".into(),
                    arguments: "{\"command\":\"cargo check\"}".into(),
                    parallel: false,
                },
            ));
            seq += 1;
            t += step;
            out.push(ev(
                seq,
                t,
                RuntimeEvent::ToolCallCompleted {
                    id: call,
                    ok: true,
                    preview: "checking...".into(),
                    duration_ms: 400,
                    applied_diff: None,
                    exit_code: Some(0),
                    stop: None,
                },
            ));
            seq += 1;
        }
        if i % 4 == 0 {
            let text = format!("Progress paragraph {i} with some detail.\n\n");
            for chunk in text.as_bytes().chunks(16) {
                t += step;
                out.push(ev(
                    seq,
                    t,
                    RuntimeEvent::AssistantTextDelta {
                        message_id: mid.clone(),
                        delta: String::from_utf8_lossy(chunk).into_owned(),
                    },
                ));
                seq += 1;
            }
        }
    }
    out
}

fn synth_long_history(turns: usize) -> Vec<crate::record::RecordedEvent> {
    let mut out = Vec::new();
    let mut seq = 0u64;
    let mut t = 0u64;
    for i in 0..turns {
        out.push(ev(
            seq,
            t,
            RuntimeEvent::UserMessageAdded {
                message: ui_user(i),
            },
        ));
        seq += 1;
        let mid = MessageId::new(format!("a{i}"));
        out.push(ev(
            seq,
            t,
            RuntimeEvent::AssistantMessageStarted {
                message_id: mid.clone(),
            },
        ));
        seq += 1;
        let text = long_markdown(1_500);
        for chunk in text.as_bytes().chunks(48) {
            t += 5_000;
            out.push(ev(
                seq,
                t,
                RuntimeEvent::AssistantTextDelta {
                    message_id: mid.clone(),
                    delta: String::from_utf8_lossy(chunk).into_owned(),
                },
            ));
            seq += 1;
        }
        out.push(ev(
            seq,
            t,
            RuntimeEvent::AssistantMessageCompleted {
                message_id: mid.clone(),
            },
        ));
        seq += 1;
    }
    // Now stream a long final answer into the same long history.
    let mid = MessageId::new("live");
    out.push(ev(
        seq,
        t,
        RuntimeEvent::AssistantMessageStarted {
            message_id: mid.clone(),
        },
    ));
    seq += 1;
    let text = long_markdown(8_000);
    for chunk in text.as_bytes().chunks(32) {
        t += 20_000;
        out.push(ev(
            seq,
            t,
            RuntimeEvent::AssistantTextDelta {
                message_id: mid.clone(),
                delta: String::from_utf8_lossy(chunk).into_owned(),
            },
        ));
        seq += 1;
    }
    out
}
