//! Replay a CORPUS of real recorded sessions and assert what the screen said.
//!
//! `real_session_replay.rs` replays one session and prints it, for a person to
//! read. This one replays many and checks them, so a regression in the
//! projection is caught without anybody reading a frame.
//!
//! Everything here comes off disk. Nothing is written, no event is
//! constructed, and no shape is imagined: each session's durable `EngineEvent`
//! log goes through the real `EventBridge`, the real reducer and the real
//! renderer, exactly as the live client consumed it.
//!
//! Two things a log alone cannot give, and how each is obtained honestly:
//!
//! * The opening `SessionOpened` snapshot is rebuilt from the session row.
//! * The turn-end marker is not on the client event stream at all — the live
//!   client learns it from the call's return value. It is rebuilt here from the
//!   durable `TaskFinished { stop, reason }` through
//!   [`leveler_app::event_bridge::turn_end_event`], the same mapping the live
//!   client used. Without it the replay never reaches terminal state, and
//!   "blocked" cannot be told from "done" — the one thing most worth checking.
//!
//! ```text
//! REPLAY_CORPUS=<file listing label<TAB>path/to/sessions.db, or a directory> \
//! REPLAY_OUT=/tmp/corpus.json \
//!   cargo test -p leveler-cli --test real_session_corpus -- --ignored --nocapture
//! ```

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use leveler_app::event_bridge::{EventBridge, turn_end_event};
use leveler_client_protocol::{RuntimeEvent, SessionId, UiSessionSnapshot};
use leveler_engine::EngineEvent;
use leveler_storage::{Database, EventStore, SessionRepository};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const W: u16 = 100;
const H: u16 = 34;
/// Roughly how many frames to render per session. Every event would be
/// quadratic on a 4000-event log and buys nothing: the screen is cumulative.
const FRAMES_PER_SESSION: usize = 60;

/// The success mark. It may only stand for work that succeeded.
const TICK: char = '✓';
/// Product wording for an outcome that is NOT success. A line carrying one of
/// these must never also carry the tick.
const NOT_SUCCESS: &[&str] = &[
    "受阻",
    "未完成",
    "已停止",
    "已取消",
    "失败",
    "验证未通过",
    "阻塞",
];
/// An internal identifier that must never reach a person's screen.
const INTERNAL_LEAKS: &[&str] = &["compact_json", "ContextCompaction", "StopReason::"];

fn frame_lines(state: &mut AppState) -> Vec<String> {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    let mut out = Vec::new();
    for y in 0..H {
        let mut line = String::new();
        let mut x = 0u16;
        while x < W {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            line.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// Copy the store out: a live run's database carries WAL sidecars.
fn copy_db(src: &Path, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("corpus-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join("sessions.db");
    std::fs::copy(src, &dst).unwrap();
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", src.display()));
        if side.exists() {
            let _ = std::fs::copy(&side, PathBuf::from(format!("{}{suffix}", dst.display())));
        }
    }
    dst
}

fn boot() -> Boot {
    Boot {
        session_id: SessionId::new("corpus"),
        user: "麻凡".into(),
        version: "0.2.0-beta.1".into(),
        show_welcome: false,
        draft_path: None,
        history_path: None,
        context_window: 1_048_576,
        locale: leveler_tui::Locale::Zh,
        untrusted_config: Vec::new(),
        reasoning_effort: None,
    }
}

fn snapshot(id: &str, goal: &str, repo: &str, model: &str) -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new(id),
        repository: repo.into(),
        goal: goal.into(),
        model: leveler_client_protocol::ModelRef::parse(model),
        mode: leveler_client_protocol::PermissionProfile::Assisted,
        branch: Some("main".into()),
        status: "idle".into(),
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        plan: None,
        verification: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        reasoning: None,
        work_profile: None,
        collaboration: None,
    }
}

/// One replayed session: what it was, and everything the checks need.
#[derive(Default, serde::Serialize)]
struct Replayed {
    label: String,
    db: String,
    session: String,
    repository: String,
    goal: String,
    model: String,
    events: usize,
    runtime_events: usize,
    frames: usize,
    decode_failures: usize,
    tool_calls: usize,
    args_unparseable: Vec<String>,
    patch_calls: usize,
    patch_identity_lost: Vec<String>,
    new_file_calls: usize,
    applied_diffs: usize,
    /// An edit the tool reported as failed that nonetheless carries a diff of
    /// what it changed. On screen that reads as applied.
    failed_edit_looks_applied: Vec<String>,
    /// An applied diff that does not say which file it is for.
    diff_without_file: Vec<String>,
    /// Replacement characters or embedded NULs in a tool's arguments.
    invalid_utf8_args: usize,
    largest_args: Vec<(String, usize)>,
    plan_steps_max: usize,
    diff_files: usize,
    verification_states: Vec<String>,
    terminal: Option<String>,
    task_outcome: Option<String>,
    wrong_success_glyph: Vec<String>,
    internal_leak: Vec<String>,
    bare_patch_rows: usize,
    replay_millis: u128,
    /// Frame at the end of the session, kept for a human to read.
    final_frame: Vec<String>,
}

/// Facts a check needs about one tool call, taken from the durable log.
fn inspect_tool_call(name: &str, arguments: &str, r: &mut Replayed) {
    r.tool_calls += 1;
    r.largest_args
        .push((name.to_string(), arguments.chars().count()));
    // The field is a Rust `String`, so it cannot hold invalid UTF-8 — asserting
    // that would assert the type system. What CAN survive a bad decode and reach
    // a screen is a replacement character or an embedded NUL, so look for those.
    if arguments.contains('\u{fffd}') || arguments.contains('\0') {
        r.invalid_utf8_args += 1;
    }
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(arguments);
    let Ok(value) = parsed else {
        // The interface reads these to learn which file an edit touched. An
        // argument blob that no longer parses is how a filename gets lost.
        r.args_unparseable
            .push(format!("{name}: {} chars", arguments.chars().count()));
        return;
    };
    let is_patch = name == "apply_patch";
    if !is_patch {
        return;
    }
    r.patch_calls += 1;
    let patch = value
        .get("patch")
        .or_else(|| value.get("input"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if patch.contains("*** Add File:") {
        r.new_file_calls += 1;
    }
    // Identity: a patch must still say which file it is for. Losing the header
    // is exactly what cutting the serialized json used to do.
    let names_a_file = patch.contains("*** Add File:")
        || patch.contains("*** Update File:")
        || patch.contains("*** Delete File:")
        || value
            .get("path")
            .and_then(|v| v.as_str())
            .is_some_and(|p| !p.is_empty());
    if !names_a_file {
        r.patch_identity_lost.push(format!(
            "apply_patch with no file header ({} chars)",
            arguments.chars().count()
        ));
    }
}

fn check_frame(lines: &[String], r: &mut Replayed, seen: &mut HashSet<String>) {
    for line in lines {
        if line.contains(TICK)
            && let Some(word) = NOT_SUCCESS.iter().find(|w| line.contains(**w))
        {
            let claim = format!("{word}: {}", line.trim());
            if seen.insert(claim.clone()) {
                r.wrong_success_glyph.push(claim);
            }
        }
        for leak in INTERNAL_LEAKS {
            if line.contains(leak) {
                let claim = format!("{leak}: {}", line.trim());
                if seen.insert(claim.clone()) {
                    r.internal_leak.push(claim);
                }
            }
        }
        // The label a patch row falls back to when it could not name a file.
        let bare = line.trim();
        if bare.ends_with("补丁") && !bare.contains('.') && !bare.contains('/') {
            r.bare_patch_rows += 1;
        }
    }
}

/// Replay one session end to end.
async fn replay_session(
    db: &Database,
    session: &leveler_storage::SessionRecord,
    label: &str,
    db_path: &str,
    state: &mut AppState,
) -> Replayed {
    let started = std::time::Instant::now();
    let sid = SessionId::new(session.id.clone());
    let records = db
        .load_window(&sid, 1, i64::MAX)
        .await
        .expect("load the log");

    let mut r = Replayed {
        label: label.to_string(),
        db: db_path.to_string(),
        session: session.id.clone(),
        repository: session.repository.clone(),
        goal: session.goal.chars().take(120).collect(),
        model: session.model.clone(),
        events: records.len(),
        ..Default::default()
    };

    let (tx, mut rx) = tokio::sync::broadcast::channel(1 << 16);
    let mut bridge = EventBridge::new(tx);

    reduce(
        state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                &session.id,
                &session.goal,
                &session.repository,
                &session.model,
            ),
        }),
    );

    let stride = (records.len() / FRAMES_PER_SESSION).max(1);
    let mut seen = HashSet::new();
    for (i, rec) in records.iter().enumerate() {
        let Ok(event) = EngineEvent::from_payload(&rec.payload) else {
            r.decode_failures += 1;
            continue;
        };
        match &event {
            EngineEvent::ToolCallStarted {
                name, arguments, ..
            } => inspect_tool_call(name, arguments, &mut r),
            EngineEvent::ToolCallFinished {
                name,
                is_error,
                applied_diff: Some(diff),
                ..
            } => {
                r.applied_diffs += 1;
                // The diff is what a person reads to see what changed. A call
                // that failed must not hand them one.
                if *is_error {
                    r.failed_edit_looks_applied
                        .push(format!("{name} reported an error and still shows a diff"));
                }
                if !diff.contains("--- ") && !diff.contains("+++ ") && !diff.contains("diff --git")
                {
                    r.diff_without_file
                        .push(format!("{name}: {} chars, no file header", diff.len()));
                }
            }
            _ => {}
        }
        // Terminal state: rebuilt from the durable row through the product's
        // own mapping, because the client stream never carried it.
        let terminal = match &event {
            EngineEvent::TaskFinished {
                outcome,
                reason,
                stop,
                ..
            } => {
                r.task_outcome = Some(format!("{outcome:?}"));
                stop.map(|s| turn_end_event(s, reason.clone()))
            }
            _ => None,
        };
        bridge.forward(event);
        while let Ok(ui) = rx.try_recv() {
            reduce(state, Action::Runtime(ui));
            r.runtime_events += 1;
        }
        if let Some(end) = terminal {
            r.terminal = Some(format!("{end:?}"));
            reduce(state, Action::Runtime(end));
        }
        if i % stride == 0 || i + 1 == records.len() {
            let lines = frame_lines(state);
            r.frames += 1;
            check_frame(&lines, &mut r, &mut seen);
        }
    }

    let lines = frame_lines(state);
    check_frame(&lines, &mut r, &mut seen);
    r.frames += 1;
    r.final_frame = lines;

    r.plan_steps_max = state.plan.as_ref().map(|p| p.steps.len()).unwrap_or(0);
    r.diff_files = state.diff.as_ref().map(|d| d.files.len()).unwrap_or(0);
    if let Some(v) = state.verification.as_ref() {
        r.verification_states = v.checks.iter().map(|c| format!("{:?}", c.status)).collect();
    }
    r.largest_args.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    r.largest_args.truncate(5);
    r.replay_millis = started.elapsed().as_millis();
    r
}

/// `label<TAB>path` lines, `#` comments, or a directory to scan.
fn corpus_entries(spec: &str) -> Vec<(String, PathBuf)> {
    let path = PathBuf::from(spec);
    if path.is_dir() {
        let mut out = Vec::new();
        let mut stack = vec![path];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.file_name().is_some_and(|n| n == "sessions.db") {
                    let label = p
                        .parent()
                        .and_then(|d| d.file_name())
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "?".into());
                    out.push((label, p));
                }
            }
        }
        out.sort();
        return out;
    }
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read corpus list {spec}: {e}"))
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.split_once('\t') {
            Some((label, p)) => (label.trim().to_string(), PathBuf::from(p.trim())),
            None => (l.to_string(), PathBuf::from(l)),
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs REPLAY_CORPUS pointing at recorded sessions"]
async fn replay_the_corpus() {
    let spec = std::env::var("REPLAY_CORPUS").expect("set REPLAY_CORPUS");
    let repeat: usize = std::env::var("REPLAY_REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let entries = corpus_entries(&spec);
    assert!(!entries.is_empty(), "corpus {spec} is empty");

    let mut all: Vec<Replayed> = Vec::new();
    // ONE AppState for the whole corpus. A fresh state per session would make
    // the leakage check unfalsifiable — nothing can carry over from a state
    // that was just created. Reusing it means every session opens on top of the
    // last one's residue, which is both the leakage test and, over a corpus this
    // size, a memory soak.
    let mut state = AppState::new(Theme::no_color(), boot());
    for (index, (label, db_path)) in entries.iter().enumerate() {
        // Index, not label: several stores can sit under directories with the
        // same name, and sharing one scratch path between them would have a
        // copy land on a file the previous read still owned.
        let copied = copy_db(db_path, &index.to_string());
        let Ok(db) = Database::connect(&copied).await else {
            panic!("cannot open {}", db_path.display());
        };
        let sessions = SessionRepository::new(&db).list().await.expect("sessions");
        for session in &sessions {
            for pass in 0..repeat {
                let tag = if repeat > 1 {
                    format!("{label}#{pass}")
                } else {
                    label.clone()
                };
                let r = replay_session(
                    &db,
                    session,
                    &tag,
                    &db_path.display().to_string(),
                    &mut state,
                )
                .await;
                println!(
                    "{:<34} {:>5} events -> {:>5} ui, {:>3} frames, {:>5}ms  {}",
                    r.label,
                    r.events,
                    r.runtime_events,
                    r.frames,
                    r.replay_millis,
                    r.terminal.as_deref().unwrap_or("(no terminal row)")
                );
                all.push(r);
            }
        }
    }

    // ── cross-session leakage ────────────────────────────────────────────
    // Sessions replayed one after another into the same state: nothing from an
    // earlier session's repository may still be on screen under a later one.
    let mut leaks: Vec<String> = Vec::new();
    for pair in all.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        if before.repository == after.repository || before.repository.is_empty() {
            continue;
        }
        let name_of = |repo: &str| {
            Path::new(repo)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        };
        let earlier = name_of(&before.repository);
        // Two different repositories can share a last path segment — every
        // eval run of a case is called "workspace". Seeing that word under the
        // next session is not evidence of anything, and reporting it as a leak
        // was a false positive on three sessions.
        if earlier.len() < 4 || earlier == name_of(&after.repository) {
            continue;
        }
        let header = after.final_frame.first().cloned().unwrap_or_default();
        if header.contains(&earlier) {
            leaks.push(format!(
                "{} still shows {earlier} from {}",
                after.label, before.label
            ));
        }
    }

    let totals = Totals::of(&all, &leaks);
    println!("\n{totals}");

    if let Ok(out) = std::env::var("REPLAY_OUT") {
        let payload = serde_json::json!({
            "sessions": all,
            "cross_session_leaks": leaks,
            "totals": totals.map(),
        });
        std::fs::write(&out, serde_json::to_string_pretty(&payload).unwrap()).unwrap();
        println!("wrote {out}");
    }

    // ── the invariants ───────────────────────────────────────────────────
    //
    // Two kinds, and they answer to different code.
    //
    // What is IN a log was written by the binary that recorded it, and no
    // later fix can change it: a session recorded before argument bounding was
    // corrected still carries the truncated arguments it was written with, so
    // asserting on those would be asserting that history is different. Entries
    // labelled `legacy:` are exempt from the log-content checks — and carry
    // every render check.
    //
    // What is ON the screen is drawn by TODAY's renderer, whatever the age of
    // the log. Those checks apply to the whole corpus, and are the reason to
    // replay old sessions at all.
    let live: Vec<&Replayed> = all
        .iter()
        .filter(|r| !r.label.starts_with("legacy:"))
        .collect();
    let live_args: Vec<String> = live
        .iter()
        .flat_map(|r| r.args_unparseable.clone())
        .collect();
    let live_patch: Vec<String> = live
        .iter()
        .flat_map(|r| r.patch_identity_lost.clone())
        .collect();
    let live_utf8: usize = live.iter().map(|r| r.invalid_utf8_args).sum();

    assert_eq!(totals.decode_failures, 0, "events failed to decode");
    assert!(
        live_args.is_empty(),
        "tool arguments no longer parse: {live_args:?}"
    );
    assert!(
        live_patch.is_empty(),
        "a patch lost the file it was for: {live_patch:?}"
    );
    assert_eq!(
        live_utf8, 0,
        "tool arguments hold a replacement character or an embedded NUL"
    );
    assert!(
        totals.failed_edit_looks_applied.is_empty(),
        "an edit that failed still shows a diff: {:?}",
        totals.failed_edit_looks_applied
    );
    assert!(
        totals.diff_without_file.is_empty(),
        "an applied diff does not name its file: {:?}",
        totals.diff_without_file
    );
    assert!(
        totals.wrong_success_glyph.is_empty(),
        "the success mark sits on work that did not succeed: {:?}",
        totals.wrong_success_glyph
    );
    assert!(
        totals.internal_leak.is_empty(),
        "an internal identifier reached the screen: {:?}",
        totals.internal_leak
    );
    assert!(
        leaks.is_empty(),
        "state carried between sessions: {leaks:?}"
    );
}

struct Totals {
    sessions: usize,
    events: usize,
    runtime_events: usize,
    frames: usize,
    tool_calls: usize,
    patch_calls: usize,
    new_file_calls: usize,
    applied_diffs: usize,
    failed_edit_looks_applied: Vec<String>,
    diff_without_file: Vec<String>,
    decode_failures: usize,
    invalid_utf8_args: usize,
    bare_patch_rows: usize,
    args_unparseable: Vec<String>,
    patch_identity_lost: Vec<String>,
    wrong_success_glyph: Vec<String>,
    internal_leak: Vec<String>,
    terminals: BTreeMap<String, usize>,
    leaks: usize,
    millis: u128,
}

impl Totals {
    fn of(all: &[Replayed], leaks: &[String]) -> Self {
        let mut terminals: BTreeMap<String, usize> = BTreeMap::new();
        for r in all {
            let key = r
                .terminal
                .as_deref()
                .map(|t| t.split_whitespace().next().unwrap_or(t).to_string())
                .unwrap_or_else(|| "(none)".into());
            *terminals.entry(key).or_default() += 1;
        }
        Self {
            sessions: all.len(),
            events: all.iter().map(|r| r.events).sum(),
            runtime_events: all.iter().map(|r| r.runtime_events).sum(),
            frames: all.iter().map(|r| r.frames).sum(),
            tool_calls: all.iter().map(|r| r.tool_calls).sum(),
            patch_calls: all.iter().map(|r| r.patch_calls).sum(),
            new_file_calls: all.iter().map(|r| r.new_file_calls).sum(),
            applied_diffs: all.iter().map(|r| r.applied_diffs).sum(),
            failed_edit_looks_applied: all
                .iter()
                .flat_map(|r| r.failed_edit_looks_applied.clone())
                .collect(),
            diff_without_file: all
                .iter()
                .flat_map(|r| r.diff_without_file.clone())
                .collect(),
            decode_failures: all.iter().map(|r| r.decode_failures).sum(),
            invalid_utf8_args: all.iter().map(|r| r.invalid_utf8_args).sum(),
            bare_patch_rows: all.iter().map(|r| r.bare_patch_rows).sum(),
            args_unparseable: all
                .iter()
                .flat_map(|r| r.args_unparseable.clone())
                .collect(),
            patch_identity_lost: all
                .iter()
                .flat_map(|r| r.patch_identity_lost.clone())
                .collect(),
            wrong_success_glyph: all
                .iter()
                .flat_map(|r| r.wrong_success_glyph.clone())
                .collect(),
            internal_leak: all.iter().flat_map(|r| r.internal_leak.clone()).collect(),
            terminals,
            leaks: leaks.len(),
            millis: all.iter().map(|r| r.replay_millis).sum(),
        }
    }

    fn map(&self) -> serde_json::Value {
        serde_json::json!({
            "sessions": self.sessions,
            "events": self.events,
            "runtime_events": self.runtime_events,
            "frames": self.frames,
            "tool_calls": self.tool_calls,
            "patch_calls": self.patch_calls,
            "new_file_calls": self.new_file_calls,
            "applied_diffs": self.applied_diffs,
            "failed_edit_looks_applied": self.failed_edit_looks_applied,
            "diff_without_file": self.diff_without_file,
            "decode_failures": self.decode_failures,
            "invalid_utf8_args": self.invalid_utf8_args,
            "bare_patch_rows": self.bare_patch_rows,
            "args_unparseable": self.args_unparseable,
            "patch_identity_lost": self.patch_identity_lost,
            "wrong_success_glyph": self.wrong_success_glyph,
            "internal_leak": self.internal_leak,
            "terminals": self.terminals,
            "cross_session_leaks": self.leaks,
            "replay_millis": self.millis,
            "events_per_sec": if self.millis > 0 {
                (self.events as u128 * 1000 / self.millis) as u64
            } else { 0 },
        })
    }
}

impl std::fmt::Display for Totals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "sessions            {}", self.sessions)?;
        writeln!(f, "engine events       {}", self.events)?;
        writeln!(f, "runtime events      {}", self.runtime_events)?;
        writeln!(f, "frames rendered     {}", self.frames)?;
        writeln!(f, "tool calls          {}", self.tool_calls)?;
        writeln!(f, "  apply_patch       {}", self.patch_calls)?;
        writeln!(f, "  new files         {}", self.new_file_calls)?;
        writeln!(f, "applied diffs       {}", self.applied_diffs)?;
        writeln!(
            f,
            "failed edit w/ diff {}",
            self.failed_edit_looks_applied.len()
        )?;
        writeln!(f, "diff w/o file       {}", self.diff_without_file.len())?;
        writeln!(f, "decode failures     {}", self.decode_failures)?;
        writeln!(f, "args unparseable    {}", self.args_unparseable.len())?;
        writeln!(f, "patch identity lost {}", self.patch_identity_lost.len())?;
        writeln!(f, "mangled args        {}", self.invalid_utf8_args)?;
        writeln!(f, "bare patch rows     {}", self.bare_patch_rows)?;
        writeln!(f, "wrong success glyph {}", self.wrong_success_glyph.len())?;
        writeln!(f, "internal leaks      {}", self.internal_leak.len())?;
        writeln!(f, "cross-session leaks {}", self.leaks)?;
        writeln!(f, "terminal rows       {:?}", self.terminals)?;
        write!(
            f,
            "replay wall         {}ms ({} events/sec)",
            self.millis,
            if self.millis > 0 {
                self.events as u128 * 1000 / self.millis
            } else {
                0
            }
        )
    }
}
