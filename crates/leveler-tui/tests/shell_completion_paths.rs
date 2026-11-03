//! Multi-level, context-aware shell path completion, exercised through the
//! real reducer and the real filesystem provider.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::{ClientCommand, PermissionProfile, SessionId, UiSessionSnapshot};
use leveler_tui::Locale;
use leveler_tui::action::{Action, Effect};
use leveler_tui::reducer::reduce;
use leveler_tui::shell_mode::InputMode;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;

fn temp_root(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "leveler-completion-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn snapshot(repository: &std::path::Path) -> UiSessionSnapshot {
    UiSessionSnapshot {
        history: None,
        id: SessionId::new("s1"),
        repository: Some(repository.to_string_lossy().into_owned()),
        task_status: None,
        task_terminal: None,
        goal: "interactive session".to_string(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: Some("main".to_string()),
        status: "idle".to_string(),
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
        execution_task: None,
        children: Vec::new(),
    }
}

fn opened(repository: &std::path::Path) -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".to_string(),
            version: "0.1.0".to_string(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 0,
            locale: Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    reduce(
        &mut s,
        Action::Runtime(leveler_client_protocol::RuntimeEvent::SessionOpened {
            session: snapshot(repository),
        }),
    );
    s
}

fn typed(s: &mut AppState, text: &str) {
    for ch in text.chars() {
        reduce(
            s,
            Action::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty())),
        );
    }
}

fn key(s: &mut AppState, code: KeyCode) -> Vec<Effect> {
    reduce(s, Action::Key(KeyEvent::new(code, KeyModifiers::empty())))
}

/// Run the real provider and fold every result back, exactly as the event
/// loop does for `Effect::ShellComplete` (an accept or fill emits more).
fn settle(s: &mut AppState, mut queue: Vec<Effect>) {
    let mut guard = 0;
    while !queue.is_empty() {
        guard += 1;
        assert!(guard < 64, "completion never settled: {queue:?}");
        let mut next = Vec::new();
        for effect in queue {
            if let Effect::ShellComplete { request } = effect {
                let result = leveler_tui::shell_mode::complete(&request);
                next.extend(reduce(s, Action::ShellCompletionsLoaded(result)));
            }
        }
        queue = next;
    }
}

fn tab(s: &mut AppState) {
    let effects = key(s, KeyCode::Tab);
    settle(s, effects);
}

fn typed_into(repository: &std::path::Path, text: &str) -> AppState {
    let mut s = opened(repository);
    typed(&mut s, "!");
    typed(&mut s, text);
    assert_eq!(s.input_mode, InputMode::Shell);
    s
}

fn displays(s: &AppState) -> Vec<String> {
    s.completion
        .items
        .iter()
        .map(|i| i.display.clone())
        .collect()
}

/// `docs/` is a real directory in the fixture; the popup lists one level.
fn fixture(tag: &str) -> std::path::PathBuf {
    let root = temp_root(tag);
    std::fs::create_dir_all(root.join("docs/guides/deep")).unwrap();
    std::fs::write(root.join("README.md"), "readme").unwrap();
    std::fs::write(root.join("docs/brand-spec.md"), "brand").unwrap();
    std::fs::write(root.join("docs/architecture.md"), "arch").unwrap();
    std::fs::write(root.join("docs/guides/nested.md"), "nested").unwrap();
    std::fs::write(root.join("docs/guides/deep/deepest.md"), "deep").unwrap();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(root.join("scripts/build.sh"), "build").unwrap();
    std::fs::write(root.join("scripts/deploy.sh"), "deploy").unwrap();
    std::fs::create_dir_all(root.join("My Dir/inner")).unwrap();
    std::fs::write(root.join("My Dir/inner/file with space.txt"), "space").unwrap();
    std::fs::write(root.join("My Dir/top file.md"), "top").unwrap();
    root
}

#[test]
fn first_level_lists_the_working_directory() {
    let root = fixture("level1");
    let mut s = typed_into(&root, "cat ");
    tab(&mut s);
    let listed = displays(&s);
    assert!(listed.contains(&"docs/".to_string()), "{listed:?}");
    assert!(listed.contains(&"scripts/".to_string()), "{listed:?}");
    assert!(listed.contains(&"README.md".to_string()), "{listed:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_selected_directory_scopes_the_next_tab_to_its_contents() {
    let root = fixture("drill");
    let mut s = typed_into(&root, "cat ");
    tab(&mut s);
    let index = s
        .completion
        .items
        .iter()
        .position(|i| i.insert == "docs/")
        .expect("docs/ candidate");
    s.completion.selected = index;
    let accepted = key(&mut s, KeyCode::Enter);
    settle(&mut s, accepted);
    assert_eq!(s.composer.text(), "cat docs/");
    assert!(!s.completion.open, "accept closes the menu");

    tab(&mut s);
    assert!(
        s.completion.open,
        "the next Tab lists docs/: {:?}",
        s.completion
    );
    let listed = displays(&s);
    assert!(listed.contains(&"brand-spec.md".to_string()), "{listed:?}");
    assert!(listed.contains(&"guides/".to_string()), "{listed:?}");
    assert!(
        !listed.contains(&"README.md".to_string()),
        "the root listing must not leak into docs/: {listed:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn three_levels_of_directories_stay_navigable() {
    let root = fixture("deep");
    let mut s = typed_into(&root, "cat ");
    for expected in ["docs/", "guides/", "deep/"] {
        tab(&mut s);
        let index = s
            .completion
            .items
            .iter()
            .position(|i| i.display == expected)
            .unwrap_or_else(|| panic!("{expected} missing: {:?}", displays(&s)));
        s.completion.selected = index;
        let accepted = key(&mut s, KeyCode::Enter);
        settle(&mut s, accepted);
    }
    assert_eq!(s.composer.text(), "cat docs/guides/deep/");
    // The fourth level holds one file, so Tab completes it in place.
    tab(&mut s);
    assert_eq!(s.composer.text(), "cat docs/guides/deep/deepest.md");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_nested_prefix_filters_within_the_selected_directory() {
    let root = fixture("prefix");
    let mut s = typed_into(&root, "cat docs/");
    tab(&mut s);
    assert!(s.completion.open, "{:?}", displays(&s));
    typed(&mut s, "arc");
    tab(&mut s);
    assert_eq!(
        s.composer.text(),
        "cat docs/architecture.md",
        "the single match completes in place"
    );
    assert!(!s.completion.open);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn directory_names_with_spaces_escape_and_drill_down() {
    let root = fixture("spaces");
    let mut s = typed_into(&root, "cat My");
    tab(&mut s);
    assert_eq!(
        s.composer.text(),
        "cat My\\ Dir/",
        "the sole match is escaped and completed in place: {:?}",
        displays(&s)
    );

    tab(&mut s);
    let listed = displays(&s);
    assert!(listed.contains(&"inner/".to_string()), "{listed:?}");
    assert!(listed.contains(&"top file.md".to_string()), "{listed:?}");

    typed(&mut s, "to");
    tab(&mut s);
    assert_eq!(s.composer.text(), "cat My\\ Dir/top\\ file.md");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dot_relative_and_absolute_paths_resolve_against_the_shell_cwd() {
    let root = fixture("relative");
    let sub = root.join("docs");

    // `./` lists the working directory.
    let mut s = typed_into(&sub, "cat ./");
    tab(&mut s);
    assert!(
        displays(&s).contains(&"brand-spec.md".to_string()),
        "{:?}",
        displays(&s)
    );

    // `../` climbs one level.
    let mut s = typed_into(&sub, "cat ../scripts/");
    tab(&mut s);
    assert!(
        displays(&s).contains(&"build.sh".to_string()),
        "{:?}",
        displays(&s)
    );

    // Use a shell path: bare backslashes are escape characters.
    let directory = if cfg!(windows) {
        root.to_string_lossy().replace('\\', "/")
    } else {
        root.to_string_lossy().into_owned()
    };
    let absolute = format!("{directory}/scripts/bu");
    let mut s = typed_into(&root, &format!("cat {absolute}"));
    tab(&mut s);
    assert_eq!(
        s.composer.text(),
        format!("cat {directory}/scripts/build.sh")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn home_relative_paths_expand_to_home_for_listing_only() {
    // `~/` in the request must list $HOME while the inserted text keeps `~/`.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let Some(home) = home else {
        return;
    };
    let root = fixture("home");
    let mut s = typed_into(&root, "cat ~/");
    tab(&mut s);
    let listed = displays(&s);
    assert!(!listed.is_empty(), "HOME must list something: {listed:?}");
    assert!(
        s.composer.text() == "cat ~/" || s.composer.text().starts_with("cat ~/"),
        "the user's spelling survives: {:?}",
        s.composer.text()
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = home;
}

#[test]
fn consecutive_tabs_walk_a_single_candidate_down_to_the_file() {
    let root = temp_root("single");
    std::fs::create_dir_all(root.join("only/next")).unwrap();
    std::fs::write(root.join("only/next/leaf.txt"), "leaf").unwrap();
    let mut s = typed_into(&root, "cat ");
    tab(&mut s);
    tab(&mut s);
    tab(&mut s);
    assert_eq!(s.composer.text(), "cat only/next/leaf.txt");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn esc_closes_the_menu_and_keeps_the_draft_executable() {
    let root = fixture("esc");
    let mut s = typed_into(&root, "cat docs/");
    tab(&mut s);
    assert!(s.completion.open, "{:?}", displays(&s));
    key(&mut s, KeyCode::Esc);
    assert!(!s.completion.open);
    assert_eq!(s.composer.text(), "cat docs/");
    assert_eq!(s.input_mode, InputMode::Shell);
    let effects = key(&mut s, KeyCode::Enter);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Send(ClientCommand::RunUserShell { command, .. })] if command == "cat docs/"
        ),
        "Enter executes once the menu is gone: {effects:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn completion_at_a_cursor_inside_the_line_replaces_only_that_token() {
    let root = fixture("midline");
    let mut s = typed_into(&root, "cat docs/ | head");
    for _ in 0.." | head".len() {
        key(&mut s, KeyCode::Left);
    }
    // Cursor now sits at the end of `docs/`.
    tab(&mut s);
    let index = s
        .completion
        .items
        .iter()
        .position(|i| i.insert == "docs/brand-spec.md")
        .expect("brand-spec candidate");
    s.completion.selected = index;
    let accepted = key(&mut s, KeyCode::Enter);
    settle(&mut s, accepted);
    assert_eq!(s.composer.text(), "cat docs/brand-spec.md | head");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn enter_accepts_the_open_menu_and_never_executes() {
    let root = fixture("enter");
    let mut s = typed_into(&root, "cat docs/");
    tab(&mut s);
    assert!(s.completion.open);
    let effects = key(&mut s, KeyCode::Enter);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Send(ClientCommand::RunUserShell { .. }))),
        "Enter on an open menu only confirms: {effects:?}"
    );
    settle(&mut s, effects);
    assert!(s.composer.text().starts_with("cat docs/"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_path_yields_no_candidates_and_no_residue() {
    let root = fixture("missing");
    let mut s = typed_into(&root, "cat nope/");
    tab(&mut s);
    assert!(!s.completion.open, "no menu for a missing directory");
    assert!(s.completion.items.is_empty());
    assert_eq!(s.composer.text(), "cat nope/");

    let mut s = typed_into(&root, "cat nope/zzz");
    tab(&mut s);
    assert!(!s.completion.open);
    assert_eq!(s.composer.text(), "cat nope/zzz");
    let _ = std::fs::remove_dir_all(&root);
}

/// A Tab that arrives while the same draft's scan is in flight must not be
/// dropped: accepting `docs/` starts a silent refetch, and the very next Tab
/// has to open that refetch's listing instead of doing nothing.
#[test]
fn a_tab_during_an_in_flight_refetch_is_never_lost() {
    let root = fixture("inflight");
    let mut s = typed_into(&root, "cat ");
    let effects = key(&mut s, KeyCode::Tab);
    settle(&mut s, effects);
    let index = s
        .completion
        .items
        .iter()
        .position(|i| i.display == "docs/")
        .expect("docs/ candidate");
    s.completion.selected = index;
    // Accept: this closes the menu and starts the silent refetch for `docs/`.
    let accepted = key(&mut s, KeyCode::Enter);
    let refetch = accepted
        .iter()
        .filter(|e| matches!(e, Effect::ShellComplete { .. }))
        .count();
    assert_eq!(refetch, 1, "accept refetches the child scope: {accepted:?}");
    // Tab lands BEFORE that result does.
    let early = key(&mut s, KeyCode::Tab);
    assert!(early.is_empty(), "nothing is scanned twice: {early:?}");
    // Now the refetch lands. It was re-armed, so it opens the menu.
    settle(&mut s, accepted);
    assert!(
        s.completion.open,
        "the in-flight Tab opened the child listing: {:?}",
        displays(&s)
    );
    assert!(
        displays(&s).contains(&"brand-spec.md".to_string()),
        "{:?}",
        displays(&s)
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A quoted directory prefix drills like an unquoted one, and the accepted
/// file carries the quotes the command needs.
#[test]
fn a_quoted_directory_prefix_drills_and_quotes_the_file() {
    let root = fixture("quoted");
    let mut s = typed_into(&root, "cat \"My Dir/");
    tab(&mut s);
    let listed = displays(&s);
    assert!(listed.contains(&"top file.md".to_string()), "{listed:?}");
    let index = s
        .completion
        .items
        .iter()
        .position(|i| i.display == "top file.md")
        .expect("file candidate");
    s.completion.selected = index;
    let accepted = key(&mut s, KeyCode::Enter);
    settle(&mut s, accepted);
    assert_eq!(s.composer.text(), "cat \"My Dir/top file.md\"");
    let _ = std::fs::remove_dir_all(&root);
}

/// `~/` lists the user's home while the replacement keeps its tilde.
#[test]
fn home_relative_completion_keeps_the_tilde_in_the_replacement() {
    let root = fixture("homekeep");
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .map(std::path::PathBuf::from)
        .expect("user home directory");
    let name = root.file_name().unwrap().to_str().unwrap();
    let probe = home.join(name);
    std::fs::create_dir(&probe).unwrap();
    let mut s = typed_into(&root, &format!("cat ~/{name}"));
    tab(&mut s);
    std::fs::remove_dir(&probe).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(s.composer.text(), format!("cat ~/{name}/"));
}

/// A `cd`-style prefix inside a compound line completes only the segment's
/// token, at the cursor, and never rewrites its neighbours.
#[test]
fn a_second_segment_completes_its_own_token() {
    let root = fixture("segment");
    let mut s = typed_into(&root, "echo start && cat docs/arch");
    tab(&mut s);
    assert_eq!(s.composer.text(), "echo start && cat docs/architecture.md");
    let _ = std::fs::remove_dir_all(&root);
}

/// Redirect targets complete from the same provider as ordinary arguments.
#[test]
fn a_redirect_target_completes_as_a_path() {
    let root = fixture("redirect");
    let mut s = typed_into(&root, "cat README.md > docs/brand");
    tab(&mut s);
    assert_eq!(s.composer.text(), "cat README.md > docs/brand-spec.md");
    let _ = std::fs::remove_dir_all(&root);
}
