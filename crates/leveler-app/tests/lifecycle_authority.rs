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

#[test]
fn execution_start_config_is_written_through_the_engine() {
    let source = app_source("session.rs");
    assert!(
        !source.contains(".set_execution("),
        "run/chat must not mutate execution config before TaskEngine acquires ownership"
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
