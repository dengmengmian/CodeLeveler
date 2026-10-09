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
            model_notice: None,
            thinking: None,
        },
    );
    state.size = (WIDTH as u16, HEIGHT);
    let snap = UiSessionSnapshot {
        id: SessionId::new("perf"),
        repository: Some("~/perf".into()),
        task_status: None,
        task_terminal: None,
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
        thinking: None,
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
                model_step: None,
                answer_effect: None,
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
                    model_step: None,
                    answer_effect: None,
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
                    model_step: None,
                    answer_effect: None,
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

/// Complete-code workloads, with the same fixture and sampling protocol on
/// either side of a presentation change. No timing threshold or hidden body
/// limit: visibility is enforced by the non-ignored contract matrix.
#[test]
#[ignore = "manual comparable full-code performance harness; run with --ignored --nocapture"]
fn perf_full_code_visibility() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    fn report(shape: &str, cols: u16, rows: u16, stage: &str, samples: Vec<f64>) {
        println!(
            "FULL_CODE_PERF {}",
            serde_json::json!({
                "shape": shape, "cols": cols, "rows": rows, "stage": stage,
                "median_ms": median(samples.clone()), "p95_ms": pct(samples.clone(), 0.95),
                "samples_ms": samples,
            })
        );
    }
    let thousand: String = (0..1000)
        .map(|i| format!("let row_{i:04} = {i}; // visible code\n"))
        .collect();
    let giant: String = (0..1000)
        .map(|i| format!("// ROW-{i:04} {} END-{i:04}\n", "x".repeat(100)))
        .collect();
    let fixtures = [
        ("normal-markdown", long_markdown_no_code(1_000)),
        ("large-markdown", long_markdown(160_000)),
        ("thousand-lines", format!("```rust\n{thousand}```\n")),
        ("hundred-kib-code", format!("```rust\n{giant}```\n")),
        (
            "long-single-line",
            format!("```text\nBEGIN {} END\n```\n", "x".repeat(10_000)),
        ),
    ];
    for (shape, text) in fixtures {
        for (cols, rows) in [(48, 20), (80, 24), (120, 40)] {
            let mut cold = Vec::new();
            for _ in 0..5 {
                crate::markdown::highlight_cache_clear();
                let started = Instant::now();
                let _ = crate::markdown::MdDoc::parse(&text);
                cold.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            report(shape, cols, rows, "cold-parse-highlight", cold);
            let theme = Theme::no_color();
            let doc = crate::markdown::MdDoc::parse(&text);
            let area = Rect::new(0, 0, cols, rows);
            let content = crate::conversation::geometry::content_rect(area);
            let mut layout = Vec::new();
            for _ in 0..15 {
                let started = Instant::now();
                let _ = doc.to_lines(content.width as usize, &theme);
                layout.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            report(shape, cols, rows, "layout", layout);
            let mut state = opened();
            state.theme = theme;
            state.size = (cols, rows);
            stream_assistant(&mut state, "full-code", &text);
            state.conv.auto_scroll = false;
            let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
            terminal
                .draw(|frame| crate::conversation::viewport::render(frame, area, &mut state))
                .unwrap();
            let projected_rows = state.conversation_lines(content.width as usize).len();
            println!(
                "FULL_CODE_PERF_FIXTURE {}",
                serde_json::json!({
                    "shape": shape, "cols": cols, "rows": rows,
                    "bytes": text.len(), "projected_rows": projected_rows,
                })
            );
            let mut draw = Vec::new();
            let mut scroll = Vec::new();
            let mut resize = Vec::new();
            for i in 0..15 {
                let started = Instant::now();
                terminal
                    .draw(|frame| crate::conversation::viewport::render(frame, area, &mut state))
                    .unwrap();
                draw.push(started.elapsed().as_secs_f64() * 1000.0);
                let started = Instant::now();
                let key = if i % 2 == 0 {
                    KeyCode::PageDown
                } else {
                    KeyCode::PageUp
                };
                reduce(
                    &mut state,
                    Action::Key(KeyEvent::new(key, KeyModifiers::NONE)),
                );
                terminal
                    .draw(|frame| crate::conversation::viewport::render(frame, area, &mut state))
                    .unwrap();
                scroll.push(started.elapsed().as_secs_f64() * 1000.0);
                let width = if i % 2 == 0 {
                    cols.saturating_sub(8)
                } else {
                    cols.saturating_sub(4)
                };
                let resized = Rect::new(0, 0, width, rows);
                let started = Instant::now();
                reduce(&mut state, Action::Resize(width, rows));
                terminal.resize(resized).unwrap();
                terminal
                    .draw(|frame| crate::conversation::viewport::render(frame, resized, &mut state))
                    .unwrap();
                resize.push(started.elapsed().as_secs_f64() * 1000.0);
                reduce(&mut state, Action::Resize(cols, rows));
                terminal.resize(area).unwrap();
                terminal
                    .draw(|frame| crate::conversation::viewport::render(frame, area, &mut state))
                    .unwrap();
            }
            report(shape, cols, rows, "cached-viewport-draw", draw);
            report(shape, cols, rows, "page-key-and-draw", scroll);
            report(shape, cols, rows, "resize-and-draw", resize);
        }
    }
}

/// Standalone audit fixtures. All generated text is retained; no presentation
/// limit or timing acceptance threshold belongs to this measurement harness.
fn independent_audit_fixtures() -> Vec<(&'static str, String)> {
    let code = |n: usize| -> String {
        let body: String = (0..n)
            .map(|i| format!("let AUDIT_ROW_{i:05} = {i}; // END_{i:05}\n"))
            .collect();
        format!("```rust\n{body}```\n")
    };
    vec![
        ("A-short-markdown", long_markdown_no_code(1_000)),
        ("B-code-100", code(100)),
        ("C-code-1000", code(1000)),
        ("D-code-100KB", one_giant_fence(100_000)),
        ("E-markdown-160KB", long_markdown(160_000)),
        ("F-code-1MB", one_giant_fence(1_000_000)),
        (
            "G-single-10000",
            format!("```text\n{}\n```\n", "x".repeat(10_000)),
        ),
        ("H-tool-receipts", "Tool receipt audit\n".into()),
        ("I-session-history", "History audit\n".into()),
        (
            "J-unicode",
            format!("```text\n{}```\n", "中文🙂e\u{301}组合👩‍💻\n".repeat(1000)),
        ),
    ]
}

#[test]
fn independent_audit_fixture_contract() {
    let fixtures = independent_audit_fixtures();
    assert_eq!(fixtures.len(), 10);
    assert!(fixtures[5].1.len() >= 1_000_000);
    assert_eq!(
        fixtures[6].1.lines().nth(1).unwrap().chars().count(),
        10_000
    );
    assert_eq!(fixtures[1].1.lines().count(), 102);
    assert_eq!(fixtures[2].1.lines().count(), 1002);
    assert!(fixtures[9].1.contains("中文🙂e\u{301}组合👩‍💻"));
}

/// In-process latency: excludes the terminal OS write and the run-loop wakeup.
/// Cold distributions have fewer samples and must not be interpreted as a
/// reliable population p99. Raw samples make that limitation inspectable.
#[test]
#[ignore = "independent audit only; release build, --exact --ignored --nocapture --test-threads=1"]
fn perf_independent_audit() {
    // A library test does not run the CLI composition root, so explicitly
    // install only the profiler inputs instead of importing credentials or
    // unrelated host configuration into this deterministic fixture.
    let values = ["LEVELER_TUI_PROFILE", "LEVELER_TUI_PROFILE_OUT"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name.into(), value)));
    let snapshot = leveler_core::EnvSnapshot::new(
        values,
        std::env::current_dir().unwrap(),
        std::env::temp_dir(),
    );
    leveler_core::install_environment(snapshot)
        .expect("run this audit as an isolated exact test in a fresh process");
    assert!(
        crate::profile::enabled(),
        "independent audit requires the actual profiler enabled"
    );
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use std::rc::Rc;

    struct AuditSamples {
        times: Vec<f64>,
        profile_before: String,
    }
    fn report(shape: &str, cols: u16, rows: u16, stage: &str, measured: AuditSamples) {
        let samples = measured.times;
        assert!(!samples.is_empty());
        assert!(samples.iter().all(|x| x.is_finite() && *x >= 0.0));
        println!(
            "INDEPENDENT_AUDIT {}",
            serde_json::json!({
                "shape": shape, "cols": cols, "rows": rows, "stage": stage,
                "n": samples.len(), "p50_ms": pct(samples.clone(), 0.5),
                "p95_ms": pct(samples.clone(), 0.95), "p99_ms": pct(samples.clone(), 0.99),
                "samples_ms": samples, "profile_before": measured.profile_before, "profile": crate::profile::report(),
            })
        );
    }
    fn sample(n: usize, mut action: impl FnMut()) -> AuditSamples {
        // Drain setup histograms; cumulative counter deltas use this snapshot.
        let profile_before = crate::profile::report();
        let times = (0..n)
            .map(|_| {
                let start = Instant::now();
                action();
                start.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        AuditSamples {
            times,
            profile_before,
        }
    }
    let filter = std::env::var("LEVELER_AUDIT_FIXTURE").ok();
    for (shape, text) in independent_audit_fixtures() {
        if filter
            .as_ref()
            .is_some_and(|value| !shape.starts_with(value))
        {
            continue;
        }
        for (cols, rows) in [(48, 20), (80, 24), (120, 40)] {
            let theme = Theme::no_color();
            let area = Rect::new(0, 0, cols, rows);
            let width = crate::conversation::geometry::content_rect(area).width as usize;
            report(
                shape,
                cols,
                rows,
                "cold-parse-highlight",
                sample(5, || {
                    crate::markdown::highlight_cache_clear();
                    std::hint::black_box(crate::markdown::MdDoc::parse(&text));
                }),
            );
            let doc = crate::markdown::MdDoc::parse(&text);
            let before = crate::markdown::highlight_cache_stats();
            report(
                shape,
                cols,
                rows,
                "warm-parse-highlight",
                sample(30, || {
                    std::hint::black_box(crate::markdown::MdDoc::parse(&text));
                }),
            );
            let after = crate::markdown::highlight_cache_stats();
            report(
                shape,
                cols,
                rows,
                "layout-wrap",
                sample(30, || {
                    std::hint::black_box(doc.to_lines(width, &theme));
                }),
            );
            // Fenced fixtures contain only code: remove the fixed header/gutter
            // spans and compare every code byte, including Unicode graphemes.
            if text.starts_with("```") {
                let source = text
                    .lines()
                    .skip(1)
                    .take(text.lines().count() - 2)
                    .collect::<String>();
                let rendered = doc.to_lines(width, &theme);
                let header_rows = usize::from(text.lines().count() > 6);
                let visible: String = rendered
                    .iter()
                    .skip(header_rows)
                    .flat_map(|line| line.spans.iter().skip(1).map(|span| span.content.as_ref()))
                    .collect();
                assert_eq!(visible, source, "{shape}: full code at {cols}x{rows}");
            }
            let mut state = opened();
            state.size = (cols, rows);
            if shape == "I-session-history" {
                push_history(&mut state, 1000, 256);
            }
            if shape == "H-tool-receipts" {
                for i in 0..1000 {
                    let id = ToolCallId::new(format!("audit-tool-{i}"));
                    runtime(
                        &mut state,
                        RuntimeEvent::ToolCallStarted {
                            id: id.clone(),
                            name: "read_file".into(),
                            arguments: format!("{{\"path\":\"src/audit_{i}.rs\"}}"),
                            parallel: false,
                            model_step: None,
                            answer_effect: None,
                        },
                    );
                    runtime(
                        &mut state,
                        RuntimeEvent::ToolCallCompleted {
                            id,
                            ok: true,
                            preview: format!("receipt {i}"),
                            duration_ms: 1,
                            applied_diff: None,
                            exit_code: None,
                            stop: None,
                        },
                    );
                }
            }
            runtime(
                &mut state,
                RuntimeEvent::AssistantMessageStarted {
                    message_id: MessageId::new("audit-live"),
                },
            );
            runtime(
                &mut state,
                RuntimeEvent::AssistantTextDelta {
                    message_id: MessageId::new("audit-live"),
                    delta: text.clone(),
                },
            );
            state.conv.auto_scroll = false;
            let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
            report(
                shape,
                cols,
                rows,
                "initial-projection-and-draw",
                sample(1, || {
                    terminal
                        .draw(|f| crate::conversation::viewport::render(f, area, &mut state))
                        .unwrap();
                }),
            );
            let projected = state.conversation_lines(width);
            report(
                shape,
                cols,
                rows,
                "stable-viewport-draw",
                sample(100, || {
                    terminal
                        .draw(|f| crate::conversation::viewport::render(f, area, &mut state))
                        .unwrap();
                }),
            );
            assert!(Rc::ptr_eq(&projected, &state.conversation_lines(width)));
            let mut key_n = 0;
            report(
                shape,
                cols,
                rows,
                "page-key-and-draw",
                sample(100, || {
                    key_n += 1;
                    let key = if key_n % 2 == 0 {
                        KeyCode::PageUp
                    } else {
                        KeyCode::PageDown
                    };
                    reduce(
                        &mut state,
                        Action::Key(KeyEvent::new(key, KeyModifiers::NONE)),
                    );
                    terminal
                        .draw(|f| crate::conversation::viewport::render(f, area, &mut state))
                        .unwrap();
                }),
            );
            assert!(Rc::ptr_eq(&projected, &state.conversation_lines(width)));
            let mut edge_n = 0;
            report(
                shape,
                cols,
                rows,
                "top-bottom-viewport-draw",
                sample(100, || {
                    edge_n += 1;
                    // Explicit viewport offsets isolate paint cost from repeated
                    // page-key travel; real input routing is measured on a PTY.
                    state.conv.scroll = if edge_n % 2 == 0 { 0 } else { usize::MAX };
                    terminal
                        .draw(|f| crate::conversation::viewport::render(f, area, &mut state))
                        .unwrap();
                }),
            );
            let mut reflow_n = 0;
            report(
                shape,
                cols,
                rows,
                "resize-reflow-and-draw",
                sample(30, || {
                    reflow_n += 1;
                    let size = if reflow_n % 2 == 0 {
                        area
                    } else {
                        Rect::new(0, 0, cols - 4, rows)
                    };
                    reduce(&mut state, Action::Resize(size.width, size.height));
                    terminal.resize(size).unwrap();
                    terminal
                        .draw(|f| crate::conversation::viewport::render(f, size, &mut state))
                        .unwrap();
                }),
            );
            reduce(&mut state, Action::Resize(cols, rows));
            terminal.resize(area).unwrap();
            report(
                shape,
                cols,
                rows,
                "stream-prose-after-complete-block",
                sample(30, || {
                    runtime(
                        &mut state,
                        RuntimeEvent::AssistantTextDelta {
                            message_id: MessageId::new("audit-live"),
                            delta: "\nAudit streaming continuation with complete text.\n".into(),
                        },
                    );
                    terminal
                        .draw(|f| crate::conversation::viewport::render(f, area, &mut state))
                        .unwrap();
                }),
            );
            println!(
                "INDEPENDENT_AUDIT_FIXTURE {}",
                serde_json::json!({
                    "shape": shape, "cols": cols, "rows": rows, "bytes": text.len(),
                    "fnv1a64": format!("{:016x}", text.bytes().fold(0xcbf29ce484222325_u64, |h, x| (h ^ u64::from(x)).wrapping_mul(0x100000001b3))),
                    "initial_projected_rows": projected.len(), "full_code_verified": text.starts_with("```"),
                    "warm_highlight_hits": after.0 - before.0, "warm_highlight_misses": after.1 - before.1,
                    "stable_projection_reused": true, "terminal_backend": "TestBackend",
                    "initial_sample_count": 1, "cold_sample_count": 5,
                })
            );
        }
    }
    if filter.is_none() {
        for target in [100_000, 1_000_000] {
            let fenced = one_giant_fence(target);
            let body = fenced
                .strip_prefix("```rust\n")
                .unwrap()
                .strip_suffix("```\n")
                .unwrap();
            for (cols, rows) in [(48, 20), (80, 24), (120, 40)] {
                let shape = format!("growing-unclosed-code-{target}");
                let area = Rect::new(0, 0, cols, rows);
                let mut state = opened();
                state.size = (cols, rows);
                runtime(
                    &mut state,
                    RuntimeEvent::AssistantMessageStarted {
                        message_id: MessageId::new("audit-open"),
                    },
                );
                runtime(
                    &mut state,
                    RuntimeEvent::AssistantTextDelta {
                        message_id: MessageId::new("audit-open"),
                        delta: "```rust\n".into(),
                    },
                );
                let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
                let mut offset = 0;
                let mut cumulative_bytes = Vec::new();
                let samples = sample(5, || {
                    let end = (offset + body.len().div_ceil(5)).min(body.len());
                    runtime(
                        &mut state,
                        RuntimeEvent::AssistantTextDelta {
                            message_id: MessageId::new("audit-open"),
                            delta: body[offset..end].into(),
                        },
                    );
                    offset = end;
                    cumulative_bytes.push(offset);
                    terminal
                        .draw(|f| crate::conversation::viewport::render(f, area, &mut state))
                        .unwrap();
                });
                report(
                    &shape,
                    cols,
                    rows,
                    "growing-unclosed-code-delta-and-draw",
                    samples,
                );
                assert_eq!(offset, body.len());
                let doc = crate::markdown::MdDoc::parse(&format!("```rust\n{body}"));
                let width = crate::conversation::geometry::content_rect(area).width as usize;
                let projected = doc.to_lines(width, &Theme::no_color());
                let visible: String = projected
                    .iter()
                    .skip(1)
                    .flat_map(|line| line.spans.iter().skip(1).map(|span| span.content.as_ref()))
                    .collect();
                assert_eq!(visible, body.replace('\n', ""));
                println!(
                    "INDEPENDENT_AUDIT_FIXTURE {}",
                    serde_json::json!({
                        "shape": shape, "cols": cols, "rows": rows, "bytes": body.len(),
                        "cumulative_body_bytes_per_sample": cumulative_bytes,
                        "distribution_semantics": "five increasing prefixes, not repeated equal-size frames",
                        "full_code_verified": true, "projected_rows": projected.len(),
                    })
                );
            }
        }
        for n in [10, 100, 1000] {
            for (cols, rows) in [(48, 20), (80, 24), (120, 40)] {
                let shape = format!("N-history-{n}");
                let mut state = opened();
                state.size = (cols, rows);
                push_history(&mut state, n, 256);
                runtime(
                    &mut state,
                    RuntimeEvent::AssistantMessageStarted {
                        message_id: MessageId::new("audit-growth"),
                    },
                );
                runtime(
                    &mut state,
                    RuntimeEvent::AssistantTextDelta {
                        message_id: MessageId::new("audit-growth"),
                        delta: "Growth audit".into(),
                    },
                );
                let area = Rect::new(0, 0, cols, rows);
                let mut terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
                report(
                    &shape,
                    cols,
                    rows,
                    "history-cold-build-and-draw",
                    sample(1, || {
                        terminal
                            .draw(|f| crate::conversation::viewport::render(f, area, &mut state))
                            .unwrap();
                    }),
                );
                report(
                    &shape,
                    cols,
                    rows,
                    "history-delta-and-draw",
                    sample(30, || {
                        runtime(
                            &mut state,
                            RuntimeEvent::AssistantTextDelta {
                                message_id: MessageId::new("audit-growth"),
                                delta: " growth".into(),
                            },
                        );
                        terminal
                            .draw(|f| crate::conversation::viewport::render(f, area, &mut state))
                            .unwrap();
                    }),
                );
            }
        }
    }
    crate::profile::emit_report();
}

/// Measure retained cache payload separately from source bytes and process RSS.
/// Run only as an exact ignored test in a fresh process; snapshots are outside
/// timed parse/layout regions and do not claim allocator or Syntect private heap.
#[test]
#[ignore = "Gate C owned-capacity audit; exact fresh process, --nocapture --test-threads=1"]
fn perf_highlight_owned_capacity_audit() {
    let values = ["LEVELER_TUI_PROFILE", "LEVELER_TUI_PROFILE_OUT"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name.into(), value)));
    leveler_core::install_environment(leveler_core::EnvSnapshot::new(
        values,
        std::env::current_dir().unwrap(),
        std::env::temp_dir(),
    ))
    .expect("run the owned-capacity audit alone in a fresh process");
    assert!(crate::profile::enabled(), "enable the actual profiler");
    let emit = |shape: &str,
                stage: &str,
                text: &str,
                samples_ms: Vec<f64>,
                before: ((u64, u64), String),
                docs: &[&crate::markdown::MdDoc]| {
        let after = crate::markdown::highlight_cache_stats();
        println!(
            "CACHE_OWNED_AUDIT {}",
            serde_json::json!({
                "shape": shape, "stage": stage, "utf8_bytes": text.len(),
                "fnv1a64": format!("{:016x}", text.bytes().fold(0xcbf29ce484222325_u64, |h, x| (h ^ u64::from(x)).wrapping_mul(0x100000001b3))),
                "samples_ms": samples_ms, "cache_hits": after.0-before.0.0,
                "cache_misses": after.1-before.0.1,
                "profile_before": before.1, "profile": crate::profile::report(),
                "profile_contract": "counter deltas are stage local; before drains earlier histogram samples, after histograms cover only this operation",
                "layout_widths": if stage == "held-doc-layout-three-widths" { Some([48,80,120]) } else { None },
                "layout_contract": "each layout sample corresponds in order to a different content width; not terminal resize and not an equal-workload pooled distribution",
                "owned_capacity": crate::markdown::highlight_cache_owned_snapshot(docs),
                "held_docs": docs.len(),
                "timing_contract": "parse/layout only; capacity snapshot outside timed region",
                "memory_contract": "cache/active/held-code payload capacities; not whole MdDoc, allocator or Syntect heap",
            })
        );
    };
    for (shape, text) in independent_audit_fixtures()
        .into_iter()
        .filter(|(shape, _)| matches!(shape.as_bytes()[0], b'B' | b'C' | b'D' | b'E' | b'F'))
    {
        crate::markdown::highlight_cache_clear();
        let before = (
            crate::markdown::highlight_cache_stats(),
            crate::profile::report(),
        );
        let started = Instant::now();
        let doc = crate::markdown::MdDoc::parse(&text);
        emit(
            shape,
            "cold",
            &text,
            vec![started.elapsed().as_secs_f64() * 1000.0],
            before,
            &[&doc],
        );
        let before = (
            crate::markdown::highlight_cache_stats(),
            crate::profile::report(),
        );
        let warm: Vec<_> = (0..30)
            .map(|_| {
                let started = Instant::now();
                std::hint::black_box(crate::markdown::MdDoc::parse(&text));
                started.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        emit(shape, "warm-attempt-30", &text, warm, before, &[&doc]);
        let before = (
            crate::markdown::highlight_cache_stats(),
            crate::profile::report(),
        );
        let resized: Vec<_> = [48usize, 80, 120]
            .into_iter()
            .map(|width| {
                let started = Instant::now();
                std::hint::black_box(doc.to_lines(width, &Theme::no_color()));
                started.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        emit(
            shape,
            "held-doc-layout-three-widths",
            &text,
            resized,
            before,
            &[&doc],
        );
        crate::markdown::highlight_cache_clear();
        let before = (
            crate::markdown::highlight_cache_stats(),
            crate::profile::report(),
        );
        emit(
            shape,
            "cache-cleared-doc-still-held",
            &text,
            vec![],
            before,
            &[&doc],
        );
    }
    let stable = long_markdown(160_000);
    assert_eq!(stable.matches("```rust\n").count(), 574);
    let giant = one_giant_fence(1_000_000);
    let body = giant
        .strip_prefix("```rust\n")
        .unwrap()
        .strip_suffix("```\n")
        .unwrap();
    let prefixes: Vec<_> = (1..=5)
        .map(|i| format!("```rust\n{}", &body[..body.len() * i / 5]))
        .collect();
    crate::markdown::highlight_cache_clear();
    let mut held = Vec::new();
    for (i, prefix) in prefixes.iter().enumerate() {
        let before = (
            crate::markdown::highlight_cache_stats(),
            crate::profile::report(),
        );
        let started = Instant::now();
        held.push(crate::markdown::MdDoc::parse(prefix));
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        emit(
            "F-growing-prefix",
            &format!("prefix-{}", i + 1),
            prefix,
            vec![ms],
            before,
            &held.iter().collect::<Vec<_>>(),
        );
    }
    for (i, prefix) in prefixes.iter().enumerate() {
        let before = (
            crate::markdown::highlight_cache_stats(),
            crate::profile::report(),
        );
        let started = Instant::now();
        std::hint::black_box(crate::markdown::MdDoc::parse(prefix));
        emit(
            "F-growing-prefix",
            &format!("exact-replay-{}", i + 1),
            prefix,
            vec![started.elapsed().as_secs_f64() * 1000.0],
            before,
            &held.iter().collect::<Vec<_>>(),
        );
    }
    drop(held);
    // Re-enter two real documents rather than only appending to one fence.
    // This catches eviction of the small working set by a large entry, and
    // eviction of a cached large entry by the 574 sequential small blocks.
    crate::markdown::highlight_cache_clear();
    let before = (
        crate::markdown::highlight_cache_stats(),
        crate::profile::report(),
    );
    let stable_doc = crate::markdown::MdDoc::parse(&stable);
    let giant_doc = crate::markdown::MdDoc::parse(&giant);
    let mixed = format!("{stable}\n{giant}");
    emit(
        "E-plus-F",
        "mixed-history-setup",
        &mixed,
        vec![],
        before,
        &[&stable_doc, &giant_doc],
    );
    for turn in 0..30 {
        let before = (
            crate::markdown::highlight_cache_stats(),
            crate::profile::report(),
        );
        let started = Instant::now();
        std::hint::black_box(crate::markdown::MdDoc::parse(&stable));
        std::hint::black_box(crate::markdown::MdDoc::parse(&giant));
        emit(
            "E-plus-F",
            &format!("history-reentry-{}", turn + 1),
            &mixed,
            vec![started.elapsed().as_secs_f64() * 1000.0],
            before,
            &[&stable_doc, &giant_doc],
        );
    }
    crate::markdown::highlight_cache_clear();
    let before = (
        crate::markdown::highlight_cache_stats(),
        crate::profile::report(),
    );
    emit(
        "E-plus-F",
        "cache-cleared-history-still-held",
        &mixed,
        vec![],
        before,
        &[&stable_doc, &giant_doc],
    );
}
