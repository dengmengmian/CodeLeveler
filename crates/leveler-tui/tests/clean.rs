//! `/clean` page tests: the slash command opens it, the async results fold in,
//! only safe actions are reachable, and both languages render.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::SessionId;
use leveler_tui::action::{Action, Effect};
use leveler_tui::clean::{
    CleanCategory, CleanKind, CleanPlanView, CleanResultView, CleanSafety, CleanStage,
};
use leveler_tui::reducer::reduce;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn state(locale: leveler_tui::Locale) -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "tester".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    )
}

fn key(code: KeyCode) -> Action {
    Action::Key(KeyEvent::new(code, KeyModifiers::empty()))
}

fn plan() -> CleanPlanView {
    CleanPlanView {
        categories: vec![
            CleanCategory {
                kind: CleanKind::ToolCache,
                bytes: 3_300_000_000,
                count: 4,
                safety: CleanSafety::Safe,
            },
            CleanCategory {
                kind: CleanKind::HistoricalAutomation,
                bytes: 9_000_000,
                count: 2,
                safety: CleanSafety::Safe,
            },
            CleanCategory {
                kind: CleanKind::DeletedProjectData,
                bytes: 156_000,
                count: 1,
                safety: CleanSafety::NeedsConfirmation,
            },
        ],
        safe_bytes: 3_309_000_000,
        needs_confirmation_bytes: 156_000,
    }
}

fn rendered(state: &mut AppState, w: u16, h: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| leveler_tui::render::render(f, state))
        .unwrap();
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

// TC1 — `/clean` opens the page and starts an off-thread scan.
#[test]
fn slash_clean_opens_the_page_and_starts_a_scan() {
    let mut s = state(leveler_tui::Locale::Zh);
    s.composer.replace("/clean");
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.active_screen, Screen::Clean);
    assert_eq!(s.clean.stage, CleanStage::Scanning);
    assert!(
        matches!(effects.as_slice(), [Effect::StartCleanScan]),
        "{effects:?}"
    );
}

// The scan result folds into the page; the plan is real, not fabricated.
#[test]
fn a_finished_scan_populates_the_plan() {
    let mut s = state(leveler_tui::Locale::Zh);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, Action::CleanScanned(Ok(plan())));
    assert_eq!(s.clean.stage, CleanStage::Ready);
    assert_eq!(s.clean.plan.as_ref().unwrap().safe_bytes, 3_309_000_000);
}

// P2 — the page offers Clean safe items and can only reach the safe subset.
#[test]
fn clean_safe_emits_the_run_effect_and_never_a_confirmation_effect() {
    let mut s = state(leveler_tui::Locale::Zh);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, Action::CleanScanned(Ok(plan())));
    let effects = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.clean.stage, CleanStage::Running);
    assert!(
        matches!(effects.as_slice(), [Effect::RunCleanSafe]),
        "{effects:?}"
    );
}

// The result folds in with real reclaimed bytes.
#[test]
fn a_finished_cleanup_shows_real_numbers() {
    let mut s = state(leveler_tui::Locale::Zh);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, Action::CleanScanned(Ok(plan())));
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(
        &mut s,
        Action::CleanRan(Ok(CleanResultView {
            reclaimed_bytes: 3_309_000_000,
            removed: 7,
            by_kind: vec![
                (CleanKind::ToolCache, 3_300_000_000),
                (CleanKind::HistoricalAutomation, 9_000_000),
            ],
            failures: Vec::new(),
        })),
    );
    assert_eq!(s.clean.stage, CleanStage::Done);
    let text = rendered(&mut s, 100, 30);
    assert!(text.contains("3.1 GB"), "{text}");
}

// Details only show sections the plan actually has.
#[test]
fn details_list_safe_and_confirmation_sections() {
    let mut s = state(leveler_tui::Locale::Zh);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, Action::CleanScanned(Ok(plan())));
    // Move to "View details" and confirm.
    let _ = reduce(&mut s, key(KeyCode::Down));
    let _ = reduce(&mut s, key(KeyCode::Enter));
    assert!(s.clean.details);
    let text = rendered(&mut s, 100, 30);
    assert!(text.contains("安全清理"), "{text}");
    assert!(text.contains("需要确认"), "{text}");
    assert!(text.contains("工具缓存"), "{text}");
    assert!(text.contains("已删除项目数据"), "{text}");
    assert!(text.contains("可能包含会话或用户数据"), "{text}");
}

// TC6 — Esc leaves the page without cancelling anything.
#[test]
fn esc_leaves_the_page_without_cancelling_work() {
    let mut s = state(leveler_tui::Locale::Zh);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, Action::CleanScanned(Ok(plan())));
    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(
        effects.is_empty(),
        "Esc must not emit a cancel: {effects:?}"
    );
}

// TC7 — Esc while a cleanup runs does not cancel or corrupt it.
#[test]
fn esc_during_cleanup_leaves_the_run_intact() {
    let mut s = state(leveler_tui::Locale::Zh);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, Action::CleanScanned(Ok(plan())));
    let _ = reduce(&mut s, key(KeyCode::Enter));
    assert_eq!(s.clean.stage, CleanStage::Running);

    let effects = reduce(&mut s, key(KeyCode::Esc));
    assert_eq!(s.active_screen, Screen::Conversation);
    assert!(effects.is_empty(), "{effects:?}");
    // The page state is untouched; the result still folds in.
    assert_eq!(s.clean.stage, CleanStage::Running);
    reduce(
        &mut s,
        Action::CleanRan(Ok(CleanResultView {
            reclaimed_bytes: 1,
            removed: 1,
            by_kind: vec![],
            failures: vec![],
        })),
    );
    assert_eq!(s.clean.stage, CleanStage::Done);
}

// TC9 — Chinese copy.
#[test]
fn chinese_render_is_complete() {
    let mut s = state(leveler_tui::Locale::Zh);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, Action::CleanScanned(Ok(plan())));
    let text = rendered(&mut s, 100, 30);
    assert!(text.contains("可释放空间"), "{text}");
    assert!(text.contains("安全可清理总计"), "{text}");
    assert!(text.contains("安全清理"), "{text}");
    assert!(text.contains("查看详情"), "{text}");
    assert!(text.contains("关闭"), "{text}");
}

// TC10 — English copy.
#[test]
fn english_render_is_complete() {
    let mut s = state(leveler_tui::Locale::En);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, Action::CleanScanned(Ok(plan())));
    let text = rendered(&mut s, 100, 30);
    assert!(text.contains("Reclaimable storage"), "{text}");
    assert!(text.contains("Safe to reclaim"), "{text}");
    assert!(text.contains("Clean safe items"), "{text}");
    assert!(text.contains("View details"), "{text}");
    assert!(text.contains("Needs confirmation"), "{text}");
}

// The scan phase renders a real "analyzing" state, not a blank page.
#[test]
fn scanning_phase_renders() {
    let mut s = state(leveler_tui::Locale::En);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    let text = rendered(&mut s, 100, 20);
    assert!(text.contains("Analyzing reclaimable storage"), "{text}");
}

// V1 — the user never sees internal "runtime" jargon on this page.
#[test]
fn no_internal_runtime_jargon_on_the_page() {
    let mut s = state(leveler_tui::Locale::Zh);
    s.composer.replace("/clean");
    let _ = reduce(&mut s, key(KeyCode::Enter));
    reduce(&mut s, Action::CleanScanned(Ok(plan())));
    let text = rendered(&mut s, 120, 30);
    assert!(!text.contains("旧运行时"), "{text}");
    assert!(!text.contains("previous runtime"), "{text}");
}
