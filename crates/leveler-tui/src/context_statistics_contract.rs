//! Cross-surface contract for context statistics.
//!
//! `testdata/context_statistics/v1/` is the ONE corpus the terminal, the Web
//! client and the desktop read. Each case is a runtime `ContextAccounting` (or
//! its absence) plus the provider usage and the declared model window; the
//! expected semantics are the compaction axis (projected input over the
//! effective input capacity), the soft threshold, the hard capacity and the
//! fold state.
//!
//! This test pins the terminal's rendering of that corpus. It never derives a
//! threshold: `compact_at_tokens` and `input_capacity_tokens` come from the
//! fixture exactly as the runtime published them.

use std::path::Path;

use crate::state::{AppState, Boot};
use crate::theme::Theme;

#[derive(serde::Deserialize)]
struct Provider {
    input: u64,
    output: u64,
}

#[derive(serde::Deserialize, Default)]
struct Render {
    /// The exact footer chip the case must produce (`None` = no gauge).
    tui_footer: Option<String>,
}

#[derive(serde::Deserialize)]
struct Expect {
    shown: bool,
    #[serde(default)]
    render: Render,
}

#[derive(serde::Deserialize)]
struct Case {
    id: String,
    /// Absent when the runtime has no live accounting for the session.
    accounting: Option<leveler_model::ContextAccounting>,
    provider: Provider,
    model_window: Option<u64>,
    expect: Expect,
}

fn test_state() -> AppState {
    AppState::new(
        Theme::no_color(),
        Boot {
            session_id: leveler_core::SessionId::new("s1"),
            user: "tester".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: crate::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    )
}

fn cases() -> Vec<Case> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/context_statistics/v1");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .map(|entry| entry.expect("readable dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "the context statistics corpus is empty");
    paths
        .iter()
        .map(|path| {
            let raw = std::fs::read_to_string(path).expect("readable case");
            serde_json::from_str(&raw).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        })
        .collect()
}

#[test]
fn the_footer_chip_matches_the_shared_context_statistics_contract() {
    for case in cases() {
        let mut state = test_state();
        state.context.loaded = case.accounting;
        state.context_tokens = (case.provider.input + case.provider.output) as u32;
        state.token_input = case.provider.input as u32;
        state.context_window_tokens = case.model_window.unwrap_or(0) as u32;

        let chip = crate::status_line::footer_ctx_chip(&state);
        assert_eq!(
            chip.as_deref(),
            case.expect.render.tui_footer.as_deref(),
            "case {}: the gauge must be the corpus's own numbers",
            case.id
        );
        assert_eq!(
            chip.is_some(),
            case.expect.shown,
            "case {}: availability disagrees with the corpus",
            case.id
        );
    }
}
