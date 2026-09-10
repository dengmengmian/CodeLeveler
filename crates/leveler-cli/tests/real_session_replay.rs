//! Replay a REAL recorded session through the real bridge, the real reducer and
//! the real renderer, and print the frames.
//!
//! `product_scenarios.rs` in `leveler-tui` renders scenarios whose events were
//! hand-written to match real shapes. This one writes nothing: it reads the
//! durable EventLog a run actually left behind, pushes every `EngineEvent`
//! through `EventBridge` — the same projection the live client consumed — and
//! feeds the resulting `RuntimeEvent`s to the TUI. What comes out is what that
//! session looked like on screen.
//!
//! The one thing the log does not carry is the opening `SessionOpened`
//! snapshot, which the app builds from the session row; it is reconstructed
//! from that same row here.
//!
//! ```text
//! REPLAY_DB=<path to sessions.db> \
//!   cargo test -p leveler-cli --test real_session_replay -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use leveler_app::event_bridge::EventBridge;
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

fn frame(state: &mut AppState, label: &str) -> String {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    let mut out = format!("\n===== {label} =====\n");
    for y in 0..H {
        let mut line = String::new();
        let mut x = 0u16;
        while x < W {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            line.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        let l = line.trim_end();
        if !l.is_empty() {
            out.push_str(&format!("|{l}\n"));
        }
    }
    out
}

/// Copy the store out: a live run's database carries WAL sidecars.
fn copy_db(src: &PathBuf) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dst = dir.join("sessions.db");
    std::fs::copy(src, &dst).unwrap();
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", src.display()));
        if side.exists() {
            std::fs::copy(&side, PathBuf::from(format!("{}{suffix}", dst.display()))).unwrap();
        }
    }
    dst
}

fn boot() -> Boot {
    Boot {
        session_id: SessionId::new("replay"),
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

#[tokio::test]
#[ignore = "needs REPLAY_DB pointing at a recorded session"]
async fn replay_a_recorded_session() {
    let Ok(path) = std::env::var("REPLAY_DB") else {
        panic!("set REPLAY_DB to a recorded sessions.db");
    };
    let db_path = copy_db(&PathBuf::from(path));
    let db = Database::connect(&db_path).await.expect("open the store");

    let sessions = SessionRepository::new(&db).list().await.expect("sessions");
    let session = sessions.first().expect("at least one session").clone();
    let sid = SessionId::new(session.id.clone());

    let records = db
        .load_window(&sid, 1, i64::MAX)
        .await
        .expect("load the whole log");
    println!(
        "replaying {} events from session {} ({} model)",
        records.len(),
        session.id,
        session.model
    );

    // The same projection a live client consumed.
    let (tx, mut rx) = tokio::sync::broadcast::channel(1 << 16);
    let mut bridge = EventBridge::new(tx);

    let mut state = AppState::new(Theme::no_color(), boot());
    reduce(
        &mut state,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot(
                &session.id,
                &session.goal,
                &session.repository,
                &session.model,
            ),
        }),
    );

    // Sample points: the frames a person would have been looking at.
    let n = records.len();
    let marks: Vec<usize> = [n / 10, n / 4, n / 2, (n * 3) / 4, n.saturating_sub(1)]
        .into_iter()
        .filter(|m| *m > 0)
        .collect();

    let mut frames = Vec::new();
    let mut forwarded = 0usize;
    for (i, rec) in records.iter().enumerate() {
        let Ok(event) = EngineEvent::from_payload(&rec.payload) else {
            continue;
        };
        bridge.forward(event);
        while let Ok(ui) = rx.try_recv() {
            reduce(&mut state, Action::Runtime(ui));
            forwarded += 1;
        }
        if marks.contains(&i) {
            frames.push(frame(
                &mut state,
                &format!("event {}/{n} (seq {})", i + 1, rec.sequence),
            ));
        }
    }
    for f in &frames {
        println!("{f}");
    }
    println!("\n{} engine events -> {forwarded} runtime events", n);
}
