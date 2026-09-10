//! W4 — the second harness.
//!
//! `Engine != Coding Agent` is a claim about a dependency direction, and a
//! direction only stays true while something other than the coding harness
//! actually drives the engine. This file is that something: a harness small
//! enough to read in one sitting, whose payload is a string, which owns no
//! repository, runs no tool, and imports nothing from `leveler-agent`, its
//! tool surface, or its verification.
//!
//! It is a proof, not a product. Do not grow it into a framework, a plugin
//! system, or a public harness SDK — the moment it needs an abstraction the
//! coding harness does not, this stops proving anything.

use std::path::Path;
use std::sync::Arc;

use leveler_core::{RuntimeId, SessionId};
use leveler_engine::{
    EngineEvent, EventLog, ExecutionKind, NewSession, SeedRequest, TaskEngine, TranscriptSink,
    TurnFacts, TurnFailure, TurnKind, TurnPorts, TurnRunner, reap_after_restart,
};
use leveler_execution::{ApprovalDecision, ApprovalRequest, Approver, AutoClarify};
use leveler_lifecycle::{AgentState, SessionStatus, StopReason, TaskOutcome, VerificationStatus};
use leveler_model::{Message, Role};
use leveler_storage::{Database, EngineStores, SessionRepository, TurnRepository};
use tokio_util::sync::CancellationToken;

/// The second harness's own result type. Not a coding result, not a report,
/// not anything the engine has a name for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MinimalResult {
    processed: String,
}

/// Nobody is attached; a non-coding harness still has to supply the ports.
struct NoHuman;

#[async_trait::async_trait]
impl Approver for NoHuman {
    async fn decide(&self, _request: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
    fn has_human(&self) -> bool {
        false
    }
}

/// The entire second harness: one turn's worth of arbitrary work.
///
/// It uses exactly three things from the engine — emit an event, persist a
/// transcript, report facts — and nothing about what it computes is legible
/// to the engine.
async fn minimal_turn(
    input: String,
    mut ports: TurnPorts,
) -> Result<TurnFacts<MinimalResult>, TurnFailure> {
    let fail = |detail: String| TurnFailure {
        cancelled: false,
        detail,
        stale_ownership: false,
        model: None,
    };

    ports.emitter.emit(EngineEvent::AssistantMessage {
        text: format!("minimal harness accepted `{input}`"),
    });
    ports
        .sink
        .append(&[
            Message::text(Role::User, input.clone()),
            Message::text(Role::Assistant, "acknowledged"),
        ])
        .await
        .map_err(|e| fail(e.to_string()))?;
    ports.emitter.emit(EngineEvent::TokenUsage {
        input_tokens: 1,
        output_tokens: 1,
        cached_input_tokens: 0,
    });

    Ok(TurnFacts {
        stop: StopReason::Answered,
        rounds: 1,
        modified_files: Vec::new(),
        outcome: MinimalResult {
            processed: format!("processed:{input}"),
        },
    })
}

fn engine(db: &Database) -> TaskEngine {
    TaskEngine {
        stores: EngineStores::from_database(db),
        runtime_id: RuntimeId::new("rt-minimal-harness"),
    }
}

async fn open_session(db: &Database) -> SessionId {
    engine(db)
        .create_task(&NewSession {
            // No repository: this harness has no workspace, and the engine
            // must not need one.
            workspace: "/nowhere".into(),
            goal: "process alpha".into(),
            model: "mock/minimal".into(),
            mode: "read-only".into(),
            sandbox: false,
            kind: ExecutionKind::Direct,
        })
        .await
        .expect("the engine creates a session for any harness")
}

/// Run one minimal turn end to end, returning the harness payload and every
/// event the engine forwarded.
async fn run_one(
    db: &Database,
    session: &SessionId,
    input: &str,
    seed: SeedRequest,
) -> (MinimalResult, Vec<EngineEvent>) {
    let engine = engine(db);
    let token = engine
        .mark_running(session)
        .await
        .expect("ownership is acquirable");
    let log = EventLog::new_owned(db, session.clone(), token.clone());
    let runner = TurnRunner {
        stores: &engine.stores,
        token,
        session_id: session.clone(),
        log: &log,
        approver: Arc::new(NoHuman),
        clarifier: Arc::new(AutoClarify),
    };
    let cancellation = CancellationToken::new();
    let mut events = Vec::new();
    let input = input.to_string();
    let recorded = runner
        .run_turn(
            TurnKind::User,
            seed,
            // No `WorkspaceFacts`: the engine must not require a repository.
            None,
            &mut |event| events.push(event),
            cancellation.clone(),
            |ports| minimal_turn(input, ports),
        )
        .await
        .expect("the engine runs a turn for a harness it knows nothing about");
    (recorded.outcome, events)
}

/// H1 + H2 + H3 + H5: a session and a turn become durable, the engine carries
/// a payload it cannot read, and generic lifecycle events land in the log.
#[tokio::test]
async fn the_engine_runs_a_turn_for_a_harness_that_is_not_the_coding_agent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("minimal.sqlite");
    let db = Database::connect(&path).await.unwrap();
    let session = open_session(&db).await;

    let (outcome, events) = run_one(
        &db,
        &session,
        "alpha",
        SeedRequest::Fresh {
            continues_active_goal: false,
        },
    )
    .await;

    // H3: the harness's own type comes back untouched.
    assert_eq!(
        outcome,
        MinimalResult {
            processed: "processed:alpha".into()
        }
    );

    // H5: the turn's generic lifecycle events were forwarded, in order.
    let started = events
        .iter()
        .position(|e| matches!(e, EngineEvent::TurnStarted { .. }))
        .expect("the engine announces the turn it opened");
    let finished = events
        .iter()
        .position(|e| matches!(e, EngineEvent::TurnFinished { .. }))
        .expect("the engine announces the turn it closed");
    assert!(started < finished, "TurnStarted must precede TurnFinished");

    // H3 again, mechanically: nothing the engine persisted derives from the
    // harness's payload. It carried the value; it never read it.
    let rows = leveler_storage::EventStore::load(&db, &session)
        .await
        .unwrap();
    assert!(
        !rows.iter().any(|r| r.payload.contains("processed:")),
        "the engine persisted a projection of the harness's own result type; \
         `TurnFacts<T>` is carried, not interpreted"
    );

    // H1 + H2: reopen the database the way a restart would and read the
    // durable facts back.
    drop(db);
    let db = Database::connect(&path).await.unwrap();
    let reloaded = SessionRepository::new(&db)
        .get(&session)
        .await
        .unwrap()
        .expect("the session survives a reconnect");
    assert_eq!(reloaded.goal, "process alpha");
    let turns = TurnRepository::new(&db).list(&session).await.unwrap();
    assert_eq!(turns.len(), 1, "exactly one turn ran");
    assert_eq!(
        turns[0].status, "completed",
        "a turn whose harness reported facts is completed"
    );
}

/// H4: a turn the runtime never finished — kill -9, not a clean cancel — is
/// reaped into `interrupted` after a restart, stays visible, and the next
/// turn runs on the same session.
#[tokio::test]
async fn an_interrupted_turn_is_visible_after_restart_and_the_next_turn_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("minimal.sqlite");
    let db = Database::connect(&path).await.unwrap();
    let session = open_session(&db).await;

    run_one(
        &db,
        &session,
        "alpha",
        SeedRequest::Fresh {
            continues_active_goal: false,
        },
    )
    .await;

    // The crash: a turn row opened by a runtime that never came back.
    let crashing = engine(&db);
    let token = crashing.mark_running(&session).await.unwrap();
    crashing
        .stores
        .turns
        .start_owned(&token, &session, "user", None, leveler_core::now())
        .await
        .expect("a turn row opens");
    drop(crashing);
    drop(db);

    // The restart.
    let db = Database::connect(&path).await.unwrap();
    let restarted = engine(&db);
    let reaped = reap_after_restart(&restarted.stores, &restarted.runtime_id, Some(&session))
        .await
        .expect("recovery reads its own history");
    assert!(
        reaped.conflicts.is_empty(),
        "the same runtime reclaims its own task"
    );

    let turns = TurnRepository::new(&db).list(&session).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(
        turns[1].status, "interrupted",
        "the lost turn is interrupted, not silently completed or still running"
    );

    // The next turn resumes the same session.
    let (outcome, _) = run_one(&db, &session, "beta", SeedRequest::Resume).await;
    assert_eq!(
        outcome,
        MinimalResult {
            processed: "processed:beta".into()
        }
    );
    let turns = TurnRepository::new(&db).list(&session).await.unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[2].status, "completed");

    // And the engine can close the task for this harness too.
    let token = restarted.acquire_ownership(&session).await.unwrap();
    restarted
        .finish_task(
            &token,
            &session,
            TaskOutcome::Completed,
            VerificationStatus::NotRun,
            None,
            Some(StopReason::Answered),
            SessionStatus::Completed,
            AgentState::Complete,
            &mut |_| {},
        )
        .await
        .expect("the engine stamps a terminal fact for any harness");
}

/// H6: this proof is worthless if it quietly leans on the coding harness.
/// Read the file back and check.
#[test]
fn the_second_harness_does_not_reach_for_the_coding_harness() {
    let source = std::fs::read_to_string(Path::new(file!()))
        .or_else(|_| {
            std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests")
                    .join("minimal_harness.rs"),
            )
        })
        .expect("this test file is readable");
    for forbidden in ["leveler_agent", "leveler_tools", "leveler_verifier"] {
        assert!(
            !source
                .lines()
                .filter(|l| l.trim_start().starts_with("use ")
                    || l.trim_start().starts_with("leveler_"))
                .any(|l| l.contains(forbidden)),
            "the second harness imports `{forbidden}`; then it is not a second \
             harness, it is the coding harness wearing a smaller name"
        );
    }
}
