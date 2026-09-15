//! Boot authority: several boots share one runtime identity, and neither
//! taking a task over nor interrupting a running turn may rest on that
//! identity. Only a boot proven dead gives up what it held.

use std::sync::Arc;

use leveler_core::{BootId, BootLiveness, OwnerEpoch, RuntimeId, SessionId, TaskId};
use leveler_engine::{
    EngineBoot, EngineError, ReapRefusal, ReapScope, TaskEngine, reap_after_restart,
};
use leveler_storage::{
    Database, EngineStores, SessionRecord, SessionRepository, TaskOwner, TurnRecord, TurnRepository,
};
use leveler_test_support::TestBoots;

const RUNTIME: &str = "rt-shared";

struct World {
    db: Database,
    boots: Arc<TestBoots>,
}

impl World {
    async fn new() -> Self {
        Self {
            db: Database::connect_in_memory().await.unwrap(),
            boots: Arc::new(TestBoots::new()),
        }
    }

    /// An engine for `boot`. Every boot shares one runtime identity.
    fn boot(&self, boot: &str) -> TaskEngine {
        TaskEngine {
            stores: EngineStores::from_database(&self.db),
            runtime_id: RuntimeId::new(RUNTIME),
            boot: EngineBoot {
                id: BootId::new(boot),
                liveness: self.boots.clone(),
            },
        }
    }

    fn set(&self, boot: &str, liveness: BootLiveness) {
        self.boots.set(&BootId::new(boot), liveness);
    }

    async fn session(&self) -> SessionId {
        let record = SessionRecord::new("/repo", "goal", "mock/m", leveler_core::now());
        SessionRepository::new(&self.db)
            .create(&record)
            .await
            .unwrap();
        SessionId::new(record.id)
    }

    /// `boot` takes the session and starts a running turn in it.
    async fn running_turn(&self, boot: &str, session: &SessionId) -> leveler_core::OwnershipToken {
        let engine = self.boot(boot);
        let token = engine.acquire_ownership(session).await.unwrap();
        engine
            .stores
            .turns
            .start_owned(&token, session, "user", None, leveler_core::now())
            .await
            .unwrap();
        token
    }

    async fn turns(&self, session: &SessionId) -> Vec<TurnRecord> {
        TurnRepository::new(&self.db).list(session).await.unwrap()
    }

    async fn statuses(&self, session: &SessionId) -> Vec<String> {
        self.turns(session)
            .await
            .into_iter()
            .map(|turn| turn.status)
            .collect()
    }

    async fn owner(&self, session: &SessionId) -> TaskOwner {
        let stores = EngineStores::from_database(&self.db);
        let task: TaskId = stores
            .tasks
            .task_for_session(session)
            .await
            .unwrap()
            .unwrap();
        stores.ownership.current(&task).await.unwrap().unwrap()
    }
}

/// T2/T14: the same runtime identity is no authority. A live boot keeps its
/// task, its generation stays current, and its fenced writes still land.
#[tokio::test]
async fn a_live_boot_keeps_its_task_against_a_sibling_of_the_same_runtime() {
    let world = World::new().await;
    let session = world.session().await;
    let token = world.running_turn("b1", &session).await;
    world.set("b1", BootLiveness::Alive);
    let before = world.owner(&session).await;

    let refused = world.boot("b2").acquire_ownership(&session).await;
    assert!(
        matches!(refused, Err(EngineError::OwnedByLiveBoot { .. })),
        "{refused:?}"
    );
    assert_eq!(world.owner(&session).await, before);
    assert_eq!(before.boot, Some(BootId::new("b1")));

    world
        .boot("b1")
        .stores
        .turns
        .start_owned(&token, &session, "user", None, leveler_core::now())
        .await
        .expect("the live owner's generation is still current");
}

/// T9/T15: a probe that cannot tell is not a death certificate.
#[tokio::test]
async fn an_owner_of_unknown_liveness_is_neither_taken_over_nor_reaped() {
    let world = World::new().await;
    let session = world.session().await;
    world.running_turn("b1", &session).await;
    world.set("b1", BootLiveness::Unknown);
    let before = world.owner(&session).await;

    let refused = world.boot("b2").acquire_ownership(&session).await;
    assert!(
        matches!(refused, Err(EngineError::OwnershipUnknown { .. })),
        "{refused:?}"
    );
    let reap = reap_after_restart(&world.boot("b2"), None, ReapScope::EndedBoots)
        .await
        .unwrap();
    assert!(reap.events.is_empty());
    assert_eq!(reap.conflicts.len(), 1);
    assert_eq!(reap.conflicts[0].refusal, ReapRefusal::UnknownOwner);
    assert_eq!(world.statuses(&session).await, ["running"]);
    assert_eq!(world.owner(&session).await, before);
}

/// T3/T4: a dead boot's task passes to the next boot, and the next boot's
/// recovery interrupts the dead boot's turn under a fresh generation.
#[tokio::test]
async fn a_dead_boots_turn_is_interrupted_and_its_task_taken_over() {
    let world = World::new().await;
    let session = world.session().await;
    world.running_turn("b1", &session).await;
    world.set("b1", BootLiveness::Dead);
    let before = world.owner(&session).await;

    let reap = reap_after_restart(&world.boot("b2"), None, ReapScope::EndedBoots)
        .await
        .unwrap();
    assert!(reap.conflicts.is_empty(), "{:?}", reap.conflicts);
    let turns = world.turns(&session).await;
    assert_eq!(turns[0].status, "interrupted");
    assert_eq!(
        turns[0].owner_boot_id.as_deref(),
        Some("b1"),
        "an interrupted turn keeps the boot that ran it"
    );
    let after = world.owner(&session).await;
    assert_eq!(after.boot, Some(BootId::new("b2")));
    assert_eq!(after.epoch, before.epoch.next().unwrap());
}

/// T8: a turn left running before boots were recorded has no provable owner.
#[tokio::test]
async fn a_running_turn_without_a_boot_is_never_reaped_or_taken_over() {
    let world = World::new().await;
    let session = world.session().await;
    TurnRepository::new(&world.db)
        .start(&session, "user", None, leveler_core::now())
        .await
        .unwrap();

    for scope in [
        ReapScope::EndedBoots,
        ReapScope::OwnBoot,
        ReapScope::OwnAndEndedBoots,
    ] {
        reap_after_restart(&world.boot("b2"), None, scope)
            .await
            .unwrap();
    }
    let refused = world.boot("b2").acquire_ownership(&session).await;
    assert!(
        matches!(refused, Err(EngineError::OwnershipUnknown { .. })),
        "{refused:?}"
    );
    assert_eq!(world.statuses(&session).await, ["running"]);
    assert_eq!(world.owner(&session).await.epoch, OwnerEpoch::UNOWNED);
}

/// T16/T17: recovery settles exactly the dead boot's turn. Live siblings'
/// turns stay running and their generations do not move.
#[tokio::test]
async fn recovery_touches_only_the_dead_boots_session() {
    let world = World::new().await;
    let (s1, s2, s3) = (
        world.session().await,
        world.session().await,
        world.session().await,
    );
    world.running_turn("b1", &s1).await;
    world.running_turn("b2", &s2).await;
    world.running_turn("b3", &s3).await;
    world.set("b1", BootLiveness::Alive);
    world.set("b2", BootLiveness::Alive);
    world.set("b3", BootLiveness::Dead);
    let (o1, o2, o3) = (
        world.owner(&s1).await,
        world.owner(&s2).await,
        world.owner(&s3).await,
    );

    reap_after_restart(&world.boot("b4"), None, ReapScope::EndedBoots)
        .await
        .unwrap();

    assert_eq!(world.statuses(&s1).await, ["running"]);
    assert_eq!(world.statuses(&s2).await, ["running"]);
    assert_eq!(world.statuses(&s3).await, ["interrupted"]);
    assert_eq!(world.owner(&s1).await, o1);
    assert_eq!(world.owner(&s2).await, o2);
    assert_eq!(world.owner(&s3).await.epoch, o3.epoch.next().unwrap());
}

/// T18: a boot's recovery leaves its own live turns alone, and its shutdown
/// settles only them — never a live sibling's.
#[tokio::test]
async fn shutdown_settles_only_the_boots_own_turns() {
    let world = World::new().await;
    let (theirs, mine) = (world.session().await, world.session().await);
    world.running_turn("b1", &theirs).await;
    world.running_turn("b2", &mine).await;
    world.set("b1", BootLiveness::Alive);
    let their_owner = world.owner(&theirs).await;

    reap_after_restart(&world.boot("b2"), None, ReapScope::EndedBoots)
        .await
        .unwrap();
    assert_eq!(
        world.statuses(&mine).await,
        ["running"],
        "recovery must not interrupt this boot's own live turn"
    );

    let shutdown = reap_after_restart(&world.boot("b2"), None, ReapScope::OwnBoot)
        .await
        .unwrap();
    assert!(shutdown.conflicts.is_empty(), "{:?}", shutdown.conflicts);
    assert_eq!(world.statuses(&mine).await, ["interrupted"]);
    assert_eq!(world.statuses(&theirs).await, ["running"]);
    assert_eq!(world.owner(&theirs).await, their_owner);
}

/// T10: one boot reacquiring its own task keeps the ordinary generation
/// advance.
#[tokio::test]
async fn a_boot_reacquires_its_own_task() {
    let world = World::new().await;
    let session = world.session().await;
    let engine = world.boot("b1");
    let first = engine.acquire_ownership(&session).await.unwrap();
    let second = engine.acquire_ownership(&session).await.unwrap();
    assert_eq!(second.owner_epoch, first.owner_epoch.next().unwrap());
    assert_eq!(second.boot_id, BootId::new("b1"));
}

impl World {
    /// `boot` runs one whole execution in the session: acquire, a turn, the
    /// turn terminal, the task terminal.
    async fn finished_execution(
        &self,
        boot: &str,
        session: &SessionId,
    ) -> leveler_core::OwnershipToken {
        let engine = self.boot(boot);
        let token = self.running_turn(boot, session).await;
        let turn = self.turns(session).await.pop().unwrap();
        engine
            .stores
            .terminal
            .finish_turn_owned(
                &token,
                session,
                &leveler_core::TurnId::new(turn.id),
                "turn_finished",
                "{}",
                leveler_engine::TurnOutcome::Completed,
                leveler_core::now(),
            )
            .await
            .unwrap();
        engine
            .finish_task(&token, session, completed(), &mut |_| {})
            .await
            .unwrap();
        token
    }
}

fn completed() -> leveler_engine::TaskTerminal {
    leveler_engine::TaskTerminal {
        outcome: leveler_engine::TaskOutcome::Completed,
        verification: leveler_lifecycle::VerificationStatus::NotRun,
        reason: None,
        stop: None,
        status: leveler_lifecycle::SessionStatus::Completed,
        state: leveler_lifecycle::AgentState::Complete,
        goal: None,
        warnings: Vec::new(),
    }
}

/// Ownership lasts as long as an execution, not as long as the boot that ran
/// it: once its task terminal commits, a live sibling may run the next one,
/// the generation moves on, and the finished turn keeps who ran it.
#[tokio::test]
async fn a_live_boot_releases_its_task_when_its_execution_ends() {
    let world = World::new().await;
    let session = world.session().await;
    world.set("b1", BootLiveness::Alive);
    world.set("b2", BootLiveness::Alive);

    let t1 = world.finished_execution("b1", &session).await;
    assert_eq!(
        world.owner(&session).await,
        TaskOwner {
            runtime: None,
            boot: None,
            epoch: t1.owner_epoch,
        }
    );

    let t2 = world
        .boot("b2")
        .acquire_ownership(&session)
        .await
        .expect("an idle live boot does not hold the session");
    assert_eq!(t2.owner_epoch, t1.owner_epoch.next().unwrap());
    assert_eq!(
        world.turns(&session).await[0].owner_boot_id.as_deref(),
        Some("b1"),
        "the finished turn keeps the boot that ran it"
    );
    assert!(matches!(
        world
            .boot("b1")
            .stores
            .turns
            .start_owned(&t1, &session, "user", None, leveler_core::now())
            .await,
        Err(leveler_storage::OwnershipError::Stale { .. })
    ));
}

/// The same boot and a sibling take turns: each execution is its own
/// generation, strictly increasing.
#[tokio::test]
async fn boots_alternate_executions_in_one_session() {
    let world = World::new().await;
    let session = world.session().await;
    world.set("b1", BootLiveness::Alive);
    world.set("b2", BootLiveness::Alive);

    let epochs = [
        world.finished_execution("b1", &session).await,
        world.finished_execution("b2", &session).await,
        world.finished_execution("b1", &session).await,
    ]
    .map(|token| token.owner_epoch.get());
    assert_eq!(epochs, [1, 2, 3]);
    let boots: Vec<_> = world
        .turns(&session)
        .await
        .into_iter()
        .map(|turn| turn.owner_boot_id.unwrap())
        .collect();
    assert_eq!(boots, ["b1", "b2", "b1"]);
}

/// Released is not up for grabs twice: of several live siblings starting at
/// once in a session a live boot left idle, exactly one wins and only one
/// turn runs.
#[tokio::test]
async fn one_of_several_boots_takes_an_idle_session() {
    let world = World::new().await;
    let session = world.session().await;
    world.set("b1", BootLiveness::Alive);
    let boots = ["b2", "b3", "b4", "b5", "b6"];
    for boot in boots {
        world.set(boot, BootLiveness::Alive);
    }
    let released = world.finished_execution("b1", &session).await;

    let starts = boots.map(|boot| {
        let engine = world.boot(boot);
        let session = session.clone();
        async move {
            let token = engine.acquire_ownership(&session).await?;
            engine
                .stores
                .turns
                .start_owned(&token, &session, "user", None, leveler_core::now())
                .await
                .map_err(EngineError::from)?;
            Ok::<_, EngineError>(token)
        }
    });
    let results = futures::future::join_all(starts).await;
    let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    assert_eq!(winners[0].owner_epoch, released.owner_epoch.next().unwrap());
    assert_eq!(
        world.statuses(&session).await,
        ["completed", "running"],
        "exactly one new turn runs"
    );
    assert_eq!(
        world.owner(&session).await.boot,
        Some(winners[0].boot_id.clone())
    );
}

/// A late or repeated terminal of a finished execution never touches the
/// execution that followed it.
#[tokio::test]
async fn a_late_terminal_does_not_release_the_next_execution() {
    let world = World::new().await;
    let session = world.session().await;
    world.set("b1", BootLiveness::Alive);
    world.set("b2", BootLiveness::Alive);
    let t1 = world.finished_execution("b1", &session).await;
    let t2 = world.running_turn("b2", &session).await;
    let running = world.owner(&session).await;

    let late = world
        .boot("b1")
        .finish_task(&t1, &session, completed(), &mut |_| {})
        .await;
    assert!(late.is_err(), "{late:?}");
    assert_eq!(world.owner(&session).await, running);
    assert_eq!(running.epoch, t2.owner_epoch);
    assert!(matches!(
        world.boot("b1").acquire_ownership(&session).await,
        Err(EngineError::OwnedByLiveBoot { .. })
    ));
    assert_eq!(world.statuses(&session).await, ["completed", "running"]);
}

/// An ownership store that lets a rival acquire the task at the expected
/// generation just before the caller's own compare-and-swap — the loser's
/// side of a genuine race, made deterministic.
struct RivalFirst {
    inner: Arc<dyn leveler_storage::OwnershipStore>,
    rival: std::sync::Mutex<Option<(BootId, bool)>>,
}

#[async_trait::async_trait]
impl leveler_storage::OwnershipStore for RivalFirst {
    async fn current(
        &self,
        task_id: &TaskId,
    ) -> Result<Option<TaskOwner>, leveler_storage::StorageError> {
        self.inner.current(task_id).await
    }

    async fn acquire(
        &self,
        task_id: &TaskId,
        runtime: &RuntimeId,
        boot: &BootId,
        expected: OwnerEpoch,
    ) -> Result<leveler_core::OwnershipToken, leveler_storage::OwnershipError> {
        let rival = self.rival.lock().unwrap().take();
        if let Some((rival, then_release)) = rival {
            let won = self
                .inner
                .acquire(task_id, runtime, &rival, expected)
                .await
                .unwrap();
            if then_release {
                self.inner.release(&won).await.unwrap();
            }
        }
        self.inner.acquire(task_id, runtime, boot, expected).await
    }

    async fn release(
        &self,
        token: &leveler_core::OwnershipToken,
    ) -> Result<(), leveler_storage::OwnershipError> {
        self.inner.release(token).await
    }
}

impl World {
    /// `boot`'s engine, whose next acquire loses the race to `rival`.
    fn losing_to(&self, boot: &str, rival: &str, then_release: bool) -> TaskEngine {
        let mut engine = self.boot(boot);
        engine.stores.ownership = Arc::new(RivalFirst {
            inner: engine.stores.ownership.clone(),
            rival: std::sync::Mutex::new(Some((BootId::new(rival), then_release))),
        });
        engine
    }
}

/// C1/C2: losing the compare-and-swap is not a storage fault. The loser reads
/// who won and hears the same typed refusal it would have heard a moment
/// later; only the winner's generation moved.
#[tokio::test]
async fn the_loser_of_an_acquire_race_hears_who_won() {
    let world = World::new().await;
    let session = world.session().await;
    world.set("b1", BootLiveness::Alive);
    world.set("b2", BootLiveness::Alive);
    world.set("b3", BootLiveness::Unknown);
    let idle = world.finished_execution("b1", &session).await;

    let lost = world
        .losing_to("b2", "b1", false)
        .acquire_ownership(&session)
        .await;
    assert!(
        matches!(lost, Err(EngineError::OwnedByLiveBoot { .. })),
        "{lost:?}"
    );
    let won = world.owner(&session).await;
    assert_eq!(won.boot, Some(BootId::new("b1")));
    assert_eq!(won.epoch, idle.owner_epoch.next().unwrap());

    let session = world.session().await;
    let lost = world
        .losing_to("b2", "b3", false)
        .acquire_ownership(&session)
        .await;
    assert!(
        matches!(lost, Err(EngineError::OwnershipUnknown { .. })),
        "{lost:?}"
    );
}

/// §26/§29: a lost race that no refusal describes stays what it is. The
/// same boot racing itself, or a winner that already let go, is a stale
/// generation — never "another CodeLeveler process".
#[tokio::test]
async fn a_lost_race_nobody_holds_stays_stale() {
    let world = World::new().await;
    world.set("b1", BootLiveness::Alive);

    let session = world.session().await;
    let lost = world
        .losing_to("b1", "b1", false)
        .acquire_ownership(&session)
        .await;
    assert!(
        matches!(
            lost,
            Err(EngineError::Ownership(
                leveler_storage::OwnershipError::Stale { .. }
            ))
        ),
        "{lost:?}"
    );

    let session = world.session().await;
    let lost = world
        .losing_to("b2", "b1", true)
        .acquire_ownership(&session)
        .await;
    assert!(
        matches!(
            lost,
            Err(EngineError::Ownership(
                leveler_storage::OwnershipError::Stale { .. }
            ))
        ),
        "{lost:?}"
    );
}
