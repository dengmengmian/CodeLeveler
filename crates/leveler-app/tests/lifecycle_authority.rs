//! Lifecycle authority is structural: application code requests transitions
//! through `TaskEngine` and never writes the same durable facts itself.

use std::path::Path;

fn app_source(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name))
        .unwrap_or_else(|error| panic!("src/{name} must be readable: {error}"))
}

#[test]
fn session_creation_has_one_lifecycle_writer() {
    let source = app_source("session.rs");
    assert!(
        source.contains(".create_task("),
        "Application session creation must delegate to TaskEngine::create_task"
    );
    assert!(
        !source.contains("repo.create(&record)"),
        "Application must not insert a runtime session directly"
    );
}

/// The run/chat path establishes the execution config exclusively through
/// `TaskEngine::start_task`, which writes it under an ownership fence
/// (`start_execution_owned`). A file-wide ban on `.set_execution(` used to
/// stand in for that invariant, but it was a proxy: the live runtime has always
/// changed a session's config while idle through `persist_runtime_config`
/// (interactive.rs), and 9010f25 added the headless equivalent,
/// `set_persisted_permission_profile`, for `run --resume --permission`.
///
/// This test now pins the real boundary precisely:
/// - run/chat writes no execution/model/axis config itself;
/// - session creation goes through `TaskEngine::create_task`;
/// - exactly one direct config writer exists in this file, and it is the
///   documented idle-resume override.
#[test]
fn run_path_writes_execution_config_through_the_engine() {
    let source = app_source("session.rs");

    // Session creation is a TaskEngine transition.
    assert!(
        source.contains(".create_task("),
        "Application session creation must delegate to TaskEngine::create_task"
    );

    // The run/chat region (from the first run entry point to the resume entry
    // point) must not pre-write config: `start_task` owns that write.
    let run_start = source
        .find("pub async fn run_in_session(")
        .expect("run_in_session exists");
    let run_end = source
        .find("pub async fn resume_session(")
        .expect("resume_session follows the run paths");
    assert!(run_start < run_end, "run paths precede resume_session");
    let run_path = &source[run_start..run_end];
    for forbidden in [
        ".set_execution(",
        ".set_axes(",
        ".update_model(",
        "SessionStore::update_status_owned",
    ] {
        assert!(
            !run_path.contains(forbidden),
            "run/chat must not write execution config before TaskEngine acquires \
             ownership (found `{forbidden}`)"
        );
    }

    // Exactly one direct config writer, and it is the idle headless-resume
    // override — not a run path and not a second writer.
    let occurrences: Vec<usize> = source
        .match_indices(".set_execution(")
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        occurrences.len(),
        1,
        "expected one fenced/idle config writer in session.rs, found {}",
        occurrences.len()
    );
    let writer = source
        .find("pub async fn set_persisted_permission_profile(")
        .expect("the idle resume permission writer exists");
    assert!(
        occurrences[0] > writer,
        "the only direct set_execution must be set_persisted_permission_profile"
    );
}

#[test]
fn session_fork_uses_the_atomic_task_creation_boundary() {
    let source = app_source("interactive.rs");
    let fork = source
        .split_once("ClientCommand::ForkSession { session_id } => {")
        .expect("ForkSession handler exists")
        .1
        .split_once("ClientCommand::Btw")
        .expect("ForkSession handler has a following command")
        .0;
    assert!(fork.contains(".create_task("));
    assert!(
        !fork.contains("sessions.create("),
        "fork must not expose a session row without its task association"
    );
}

#[test]
fn parallel_parent_uses_engine_lifecycle_entry_points() {
    let source = app_source("parallel.rs");
    for required in [".create_task(", ".mark_running(", ".finish_task("] {
        assert!(
            source.contains(required),
            "parallel parent must call the engine lifecycle entry point `{required}`"
        );
    }
    for forbidden in [
        "SessionRecord::new",
        "TaskStore::ensure_for_session",
        "SessionStore::update_status_owned",
        "TerminalStore::finish_task_owned",
        "acquire_parallel_parent_ownership",
    ] {
        assert!(
            !source.contains(forbidden),
            "parallel parent bypasses TaskEngine through `{forbidden}`"
        );
    }
}

#[test]
fn parallel_parent_routes_post_start_errors_through_terminal_epilogue() {
    let source = app_source("parallel.rs");
    let running = source
        .find(".mark_running(")
        .expect("parent enters Running");
    let captured = source
        .find("let result: Result<ParallelEditOutcome, AppError> = async")
        .expect("fallible parallel work is captured");
    let started = source
        .find("EngineEvent::TaskStarted")
        .expect("parent start is announced");
    let cancellation = source
        .find("let result = honor_parent_cancellation(result, &cancellation)")
        .expect("parent cancellation is restored after child result folding");
    let classification = source
        .find("let terminal = match &result")
        .expect("terminal classification exists");
    let terminal = source
        .rfind(".finish_task(")
        .expect("one terminal epilogue closes the parent");
    assert!(
        running < captured
            && captured < started
            && started < cancellation
            && cancellation < classification
            && classification < terminal
    );
    assert!(
        source[captured..terminal].contains(".await;"),
        "fallible work must resolve to a Result before the terminal commit"
    );
}

#[test]
fn user_shell_acquires_ownership_through_the_engine() {
    let source = app_source("interactive.rs");
    assert!(
        !source.contains("acquire_parallel_parent_ownership"),
        "user shell must use TaskEngine::acquire_ownership instead of an App-owned duplicate"
    );
}

#[test]
fn crash_acknowledgement_acquires_ownership_through_the_engine() {
    let source = app_source("session.rs");
    for forbidden in [
        "TaskStore::ensure_for_session",
        "OwnershipStore::current",
        "OwnershipStore::acquire",
    ] {
        assert!(
            !source.contains(forbidden),
            "crash acknowledgement bypasses TaskEngine through `{forbidden}`"
        );
    }
}

/// A `/btw` side answer must be structurally incapable of joining the main
/// run: its own conversation, its own cancel handle, and no write to the
/// message store or the main turn's ownership/cancellation.
#[test]
fn btw_side_thread_cannot_touch_main_authority() {
    let source = app_source("interactive.rs");
    let spawn = source
        .split_once("fn spawn_btw(")
        .expect("spawn_btw exists")
        .1
        .split_once("fn cancel_btw(")
        .expect("spawn_btw is followed by cancel_btw")
        .0;
    for forbidden in [
        "MessageRepository",
        "append_message",
        "truncate_messages",
        "stage_turn",
        "active.admit",
        "self.active",
    ] {
        assert!(
            !spawn.contains(forbidden),
            "a side answer must not touch main authority through `{forbidden}`"
        );
    }
    assert!(
        spawn.contains("thread.cancel = Some"),
        "the side answer needs its own cancel handle"
    );
    assert!(
        spawn.contains("thread.history"),
        "the side conversation must live on the side thread"
    );
}
