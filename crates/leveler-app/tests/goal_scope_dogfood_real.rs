//! Real-provider dogfood for the structured scope contract and the input it
//! must not misread.
//!
//! Each test drives the real DeepSeek provider through the real runtime and
//! asserts on durable facts only: the goal's requirement history, the effective
//! set folded from it, the files that really appeared, and the terminal's own
//! report. Model prose is printed, never used as evidence.
//!
//! Ignored by default because they spend real tokens:
//!
//! ```text
//! set -a; . ~/.leveler/deepseek.env; set +a
//! env -u NODE_OPTIONS cargo test -p leveler-app --test goal_scope_dogfood_real \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` is required: `LEVELER_HOME` is process-wide and every
//! fixture writes its own config.

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient, RuntimeEvent};
use leveler_execution::PermissionProfile;
use leveler_lifecycle::{RequirementItem, RequirementKind, RequirementState, ScopeOperationKind};
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::{
    EngineStores, GoalRecord, GoalRequirementRecord, GoalState, SessionRepository,
};

const TIMEOUT: Duration = Duration::from_secs(900);

/// One real test at a time: `LEVELER_HOME` is process-wide and every fixture
/// installs its own config into it.
static FIXTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn write_config(home: &std::path::Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        r#"
default_model = "deepseek/deepseek-v4.1-flash-1017"

[providers.deepseek]
protocol = "openai_chat"
base_url = "https://taotoken.net/api/v1"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek-v4.1-flash-1017"]
provider = "deepseek"
model_id = "deepseek-v4.1-flash-1017"
protocol = "openai_chat"
context_window = 1048576
reliable_context = 786432
max_output_tokens = 65536
reasoning = true
reasoning_style = "thinking_flag"
supported_efforts = ["low", "high", "max"]
reasoning_effort = "low"
thinking_supports_forced_tool_choice = false
passback_reasoning_content = true
"#,
    )
    .unwrap();
}

#[derive(Default, Debug)]
struct TurnFacts {
    tools: Vec<String>,
    text: String,
    terminal: bool,
    failure: Option<String>,
    outcome: Option<String>,
}

impl TurnFacts {
    fn used(&self, tool: &str) -> bool {
        self.tools.iter().any(|name| name == tool)
    }
    fn completed(&self) -> bool {
        self.outcome.as_deref().is_some_and(|outcome| {
            outcome == "completed" || outcome.starts_with("completed_with_warnings")
        })
    }
}

async fn run_turn(
    client: &Arc<InProcessRuntimeClient>,
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEvent>,
    command: ClientCommand,
) -> TurnFacts {
    let mut facts = TurnFacts::default();
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut arguments: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    client
        .send(command)
        .await
        .expect("the runtime must accept the command");
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while tokio::time::Instant::now() < deadline && !facts.terminal {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(RuntimeEvent::ToolCallStarted {
                id,
                name,
                arguments: call_arguments,
                ..
            })) => {
                println!(
                    "  CALL {name} {}",
                    call_arguments.chars().take(500).collect::<String>()
                );
                names.insert(id.as_str().to_string(), name.clone());
                arguments.insert(id.as_str().to_string(), call_arguments);
                facts.tools.push(name);
            }
            Ok(Ok(RuntimeEvent::ToolCallCompleted {
                id, ok, preview, ..
            })) => {
                if !ok {
                    let name = names
                        .get(id.as_str())
                        .cloned()
                        .unwrap_or_else(|| "unknown".into());
                    let args = arguments.get(id.as_str()).cloned().unwrap_or_default();
                    println!(
                        "  REFUSED {name}({args}) -> {}",
                        preview.chars().take(600).collect::<String>()
                    );
                }
            }
            Ok(Ok(RuntimeEvent::Notification { level, message })) => {
                println!("  notice [{level:?}]: {message}");
            }
            Ok(Ok(RuntimeEvent::AssistantTextDelta { delta, .. })) => facts.text.push_str(&delta),
            Ok(Ok(RuntimeEvent::TurnCompleted)) => {
                facts.terminal = true;
                facts.outcome = Some("completed".into());
            }
            Ok(Ok(RuntimeEvent::TurnCompletedWithWarnings { reason, .. })) => {
                facts.terminal = true;
                facts.outcome = Some(format!("completed_with_warnings: {reason}"));
            }
            Ok(Ok(RuntimeEvent::TurnAnswered)) => {
                facts.terminal = true;
                facts.outcome = Some("answered".into());
            }
            Ok(Ok(RuntimeEvent::TurnPaused { .. })) => {
                facts.terminal = true;
                facts.outcome = Some("paused".into());
            }
            Ok(Ok(RuntimeEvent::TurnIncomplete { reason })) => {
                facts.terminal = true;
                facts.outcome = Some(format!("incomplete: {reason}"));
            }
            Ok(Ok(RuntimeEvent::TurnTruncated { error })) => {
                facts.terminal = true;
                facts.outcome = Some(format!("truncated: {error}"));
            }
            Ok(Ok(RuntimeEvent::TurnCancelled)) => {
                facts.terminal = true;
                facts.outcome = Some("interrupted".into());
            }
            Ok(Ok(RuntimeEvent::TaskCancelled)) => {
                facts.terminal = true;
                facts.outcome = Some("cancelled".into());
            }
            Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => {
                facts.terminal = true;
                facts.failure = Some(error);
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                facts.failure = Some(error.to_string());
                break;
            }
            Err(_) => break,
        }
    }
    println!(
        "  tools={:?} outcome={:?} failure={:?}\n  answer: {}",
        facts.tools,
        facts.outcome,
        facts.failure,
        facts.text.trim()
    );
    facts
}

async fn goals(app: &Application, session: &leveler_core::SessionId) -> Vec<GoalRecord> {
    let db = app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let Some(task) = stores.tasks.task_for_session(session).await.unwrap() else {
        return Vec::new();
    };
    stores.goals.for_task(&task).await.unwrap()
}

async fn requirements(
    app: &Application,
    session: &leveler_core::SessionId,
) -> Vec<RequirementItem> {
    let db = app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let Some(task) = stores.tasks.task_for_session(session).await.unwrap() else {
        return Vec::new();
    };
    let Some(goal) = stores
        .goals
        .for_task(&task)
        .await
        .unwrap()
        .into_iter()
        .next()
    else {
        return Vec::new();
    };
    stores.goals.requirements(&goal.id).await.unwrap()
}

async fn requirement_log(
    app: &Application,
    session: &leveler_core::SessionId,
) -> Vec<GoalRequirementRecord> {
    let db = app.open_database().await.unwrap();
    let stores = EngineStores::from_database(&db);
    let Some(task) = stores.tasks.task_for_session(session).await.unwrap() else {
        return Vec::new();
    };
    let Some(goal) = stores
        .goals
        .for_task(&task)
        .await
        .unwrap()
        .into_iter()
        .next()
    else {
        return Vec::new();
    };
    stores.goals.requirement_log(&goal.id).await.unwrap()
}

fn describe(items: &[RequirementItem]) -> String {
    items
        .iter()
        .map(|item| {
            format!(
                "{} kind={:?} state={:?} target={:?} text={:?}",
                item.id, item.kind, item.state, item.target, item.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n    ")
}

fn by_target<'a>(items: &'a [RequirementItem], target: &str) -> Option<&'a RequirementItem> {
    items.iter().find(|item| {
        item.target
            .as_deref()
            .is_some_and(|value| value.trim() == target)
    })
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir, Arc<Application>) {
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("LEVELER_HOME", home.path());
    }
    write_config(home.path());
    std::fs::create_dir_all(repo.path().join("configs")).unwrap();
    std::fs::create_dir_all(repo.path().join("state")).unwrap();
    leveler_test_support::git::init_repo(repo.path());
    std::fs::write(repo.path().join("README.md"), "# demo\n").unwrap();
    leveler_test_support::git::run(repo.path(), &["add", "-A"]);
    leveler_test_support::git::run(repo.path(), &["commit", "-qm", "seed"]);
    let app = Arc::new(
        Application::assemble(Layout::from_parts(
            repo.path().to_path_buf(),
            repo.path().join("configs"),
            repo.path().join("state"),
        ))
        .unwrap(),
    );
    (home, repo, app)
}

async fn session(app: &Application, model: &ModelRef, title: &str) -> leveler_core::SessionId {
    app.create_session_with_collaboration(
        model,
        title,
        PermissionProfile::FullAccess,
        leveler_agent::CollaborationMode::Chat,
    )
    .await
    .unwrap()
}

fn submit(session: &leveler_core::SessionId, content: &str) -> ClientCommand {
    ClientCommand::SubmitMessage {
        session_id: session.clone(),
        content: content.to_string(),
        attachments: Vec::new(),
    }
}

async fn axis(app: &Application, session: &leveler_core::SessionId) -> String {
    let db = app.open_database().await.unwrap();
    SessionRepository::new(&db)
        .get(session)
        .await
        .unwrap()
        .unwrap()
        .collaboration
}

// ── 1. waive / restore ───────────────────────────────────────────────────────

/// The user's own words waive a requirement and then restore it. The runtime
/// must record both transitions with their quotes, keep the item out of the
/// effective set while waived, and require it again after the restore.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn scope_waive_and_restore_survive_the_whole_goal() {
    let _guard = FIXTURE_LOCK.lock().await;
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = session(&app, &model, "waive and restore").await;
    let mut events = client.subscribe_session(&session);
    println!("D-SCOPE-WAIVE session={}", session.as_str());

    // ── 1. commit three deliverables, deliver one, stop at the user's pause ──
    let first = run_turn(
        &client,
        &mut events,
        submit(
            &session,
            "请用 commit_task 登记三项交付物：a.txt、b.txt、d.txt，每个文件内容为一行，\
             分别为 `a`、`b`、`d`。requirements 用 kind=artifact，target 分别为 a.txt、\
             b.txt、d.txt，source_quote 都逐字引用「三项交付物：a.txt、b.txt、d.txt」。\
             现在只创建 a.txt，然后调用 declare_intent 声明暂停：operations 用 pause_task，\
             boundary 用 kind=now，source_quote 逐字引用「等待我确认再继续其余两项」。\
             本回合不要创建 b.txt 或 d.txt，也不要调用 update_goal(complete)。",
        ),
    )
    .await;
    assert!(
        first.failure.is_none(),
        "the opening turn failed: {first:?}"
    );
    assert!(
        first.used("commit_task"),
        "the model must commit the task: {:?}",
        first.tools
    );
    let opened = goals(&app, &session).await;
    assert_eq!(opened.len(), 1, "{opened:?}");
    let goal_id = opened[0].id.clone();
    assert_eq!(opened[0].state, GoalState::Running, "{opened:?}");
    assert!(
        opened[0].user_paused_at.is_some(),
        "the user's pause is durable: {opened:?}"
    );
    assert!(repo.path().join("a.txt").exists(), "stage a really ran");
    assert!(!repo.path().join("b.txt").exists(), "b is withheld");
    assert!(!repo.path().join("d.txt").exists(), "d is withheld");
    let before = requirements(&app, &session).await;
    println!("requirements after the commit:\n    {}", describe(&before));
    let b_before = by_target(&before, "b.txt")
        .unwrap_or_else(|| panic!("b.txt is not a declared requirement: {}", describe(&before)))
        .clone();
    assert_eq!(b_before.state, RequirementState::Active);
    assert!(
        b_before.kind == RequirementKind::Artifact,
        "the model declared b.txt as {:?}",
        b_before.kind
    );

    // ── 2. WAIVE ─────────────────────────────────────────────────────────────
    println!("D-SCOPE-WAIVE-OP");
    let waived = run_turn(
        &client,
        &mut events,
        submit(
            &session,
            "b.txt 这一项先豁免，不用交付了。请先调用 get_goal_scope，然后调用 \
             declare_scope：op=waive，requirement_id 用 b.txt 那一项的 id，source_quote \
             逐字用「b.txt 这一项先豁免，不用交付了」。本回合不要创建任何文件，也不要 \
             调用 update_goal(complete)。",
        ),
    )
    .await;
    assert!(
        waived.failure.is_none(),
        "the waive turn failed: {waived:?}"
    );
    let after_waive = requirements(&app, &session).await;
    println!(
        "requirements after the waive:\n    {}",
        describe(&after_waive)
    );
    let b_waived =
        by_target(&after_waive, "b.txt").expect("the waived requirement is still in the history");
    assert_eq!(
        b_waived.state,
        RequirementState::Waived,
        "the user's waiver must not be guessed or dropped: {}",
        describe(&after_waive)
    );
    assert!(
        !b_waived.is_owed(),
        "a waived requirement is not owed: {}",
        describe(&after_waive)
    );
    let log = requirement_log(&app, &session).await;
    let waive_event = log
        .iter()
        .find(|record| record.event.operation == ScopeOperationKind::Waive)
        .unwrap_or_else(|| panic!("no waive event in the durable history: {log:?}"));
    assert_eq!(waive_event.event.requirement_id, b_waived.id);
    assert!(
        waive_event.event.source_quote.contains("先豁免"),
        "the event keeps the user's own authorization: {:?}",
        waive_event.event
    );
    let goal_now = goals(&app, &session).await;
    assert_eq!(goal_now[0].id, goal_id, "same goal identity");
    assert!(
        goal_now[0].user_paused_at.is_some(),
        "a waiver is not an authorization to continue: {goal_now:?}"
    );

    // ── 3. RESTORE ───────────────────────────────────────────────────────────
    println!("D-SCOPE-RESTORE-OP");
    let restored = run_turn(
        &client,
        &mut events,
        submit(
            &session,
            "重新恢复 b.txt 这一项，它仍然要交付。请调用 declare_scope：op=restore，\
             requirement_id 用 b.txt 那一项的 id，source_quote 逐字用「重新恢复 b.txt \
             这一项」。本回合不要创建任何文件，也不要调用 update_goal(complete)。",
        ),
    )
    .await;
    assert!(
        restored.failure.is_none(),
        "the restore turn failed: {restored:?}"
    );
    let after_restore = requirements(&app, &session).await;
    println!(
        "requirements after the restore:\n    {}",
        describe(&after_restore)
    );
    let b_restored =
        by_target(&after_restore, "b.txt").expect("the restored requirement still exists");
    assert_eq!(
        b_restored.id,
        b_waived.id,
        "restore revives the same identity: {}",
        describe(&after_restore)
    );
    assert_eq!(
        b_restored.state,
        RequirementState::Active,
        "restore makes the requirement owed again: {}",
        describe(&after_restore)
    );
    assert!(
        !repo.path().join("b.txt").exists(),
        "no file may appear before the user authorizes the continuation"
    );
    let log = requirement_log(&app, &session).await;
    assert!(
        log.iter()
            .any(|record| record.event.operation == ScopeOperationKind::Restore),
        "the restore is durable history: {log:?}"
    );

    // ── 4. the user authorizes the withheld work, and both items are owed ────
    println!("D-SCOPE-FINISH");
    let finished = run_turn(
        &client,
        &mut events,
        submit(
            &session,
            "确认，继续完成剩余交付物：创建 b.txt 和 d.txt（分别一行 `b`、`d`），然后调用 \
             update_goal(status=\"complete\")，按 get_goal_scope 里的 active id 把每一项记为 done。",
        ),
    )
    .await;
    assert!(
        finished.failure.is_none(),
        "the finishing turn failed: {finished:?}"
    );
    let closed = goals(&app, &session).await;
    println!("goal after the continuation: {closed:?}");
    println!("axis: {}", axis(&app, &session).await);
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0].id, goal_id, "same goal identity");
    assert_eq!(
        closed[0].state,
        GoalState::Settled,
        "the restored work really finished: {closed:?}"
    );
    assert!(
        repo.path().join("b.txt").exists(),
        "the restored requirement was delivered: {:?}",
        finished.tools
    );
    assert!(
        repo.path().join("d.txt").exists(),
        "the other withheld requirement was delivered: {:?}",
        finished.tools
    );
    let final_items = requirements(&app, &session).await;
    let b_final = by_target(&final_items, "b.txt").expect("b.txt is still recorded");
    assert_eq!(
        b_final.state,
        RequirementState::Active,
        "a delivered requirement stays active in history: {}",
        describe(&final_items)
    );
    assert!(finished.completed(), "the goal closes: {finished:?}");
}

// ── 2. replace ───────────────────────────────────────────────────────────────

/// `replace` retires the old requirement and mints a new identity carrying the
/// replacement. The old item is never silently treated as delivered.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn scope_replace_retires_the_old_requirement() {
    let _guard = FIXTURE_LOCK.lock().await;
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = session(&app, &model, "replace a requirement").await;
    let mut events = client.subscribe_session(&session);
    println!("D-SCOPE-REPLACE session={}", session.as_str());

    let first = run_turn(
        &client,
        &mut events,
        submit(
            &session,
            "请用 commit_task 登记两项交付物：a.txt（一行 `a`）和 b.txt（一行 `b`）。\
             requirements 用 kind=artifact，target 分别为 a.txt 和 b.txt，source_quote \
             逐字引用「两项交付物：a.txt 和 b.txt」。现在只创建 a.txt，然后调用 \
             declare_intent 声明暂停：operations 用 pause_task，boundary 用 kind=now，\
             source_quote 逐字引用「等待我确认再继续 b.txt」。本回合不要创建 b.txt，也不要\
             调用 update_goal(complete)。",
        ),
    )
    .await;
    assert!(
        first.failure.is_none(),
        "the opening turn failed: {first:?}"
    );
    let opened = goals(&app, &session).await;
    assert_eq!(opened.len(), 1, "{opened:?}");
    let goal_id = opened[0].id.clone();
    assert!(opened[0].user_paused_at.is_some(), "{opened:?}");
    let before = requirements(&app, &session).await;
    println!(
        "requirements before the replace:\n    {}",
        describe(&before)
    );
    let old = by_target(&before, "b.txt")
        .unwrap_or_else(|| panic!("b.txt is not declared: {}", describe(&before)))
        .clone();
    assert_eq!(old.state, RequirementState::Active);

    println!("D-SCOPE-REPLACE-OP");
    let replaced = run_turn(
        &client,
        &mut events,
        submit(
            &session,
            "b.txt 不用做了，改成创建 c.txt（内容一行 `c`）。请先调用 get_goal_scope，然后\
             调用 declare_scope：op=replace，requirement_id 用 b.txt 那一项的 id，\
             kind=artifact，target=c.txt，text=创建 c.txt，source_quote 逐字用「b.txt \
             不用做了，改成创建 c.txt」。确认继续，创建 c.txt，然后调用 \
             update_goal(status=\"complete\")，按 get_goal_scope 里的 active id 记账。",
        ),
    )
    .await;
    assert!(
        replaced.failure.is_none(),
        "the replace turn failed: {replaced:?}"
    );
    assert!(
        replaced.used("declare_scope"),
        "the model must declare the change: {:?}",
        replaced.tools
    );
    let after = requirements(&app, &session).await;
    println!("requirements after the replace:\n    {}", describe(&after));
    let retired = by_target(&after, "b.txt").expect("the retired requirement stays in history");
    assert_eq!(
        retired.id,
        old.id,
        "the replaced item keeps its identity as history: {}",
        describe(&after)
    );
    assert_eq!(
        retired.state,
        RequirementState::Superseded,
        "the old requirement is retired, not delivered: {}",
        describe(&after)
    );
    let replacement = by_target(&after, "c.txt")
        .unwrap_or_else(|| panic!("c.txt is not the replacement: {}", describe(&after)));
    assert_ne!(replacement.id, old.id, "replace mints a new identity");
    assert_eq!(replacement.state, RequirementState::Active);
    assert!(
        retired.superseded_by.as_deref() == Some(replacement.id.as_str()),
        "the retired item names its replacement: {}",
        describe(&after)
    );
    let log = requirement_log(&app, &session).await;
    let replace_event = log
        .iter()
        .find(|record| record.event.operation == ScopeOperationKind::Replace)
        .unwrap_or_else(|| panic!("no replace event in the durable history: {log:?}"));
    assert_eq!(replace_event.event.requirement_id, replacement.id);
    assert_eq!(
        replace_event.event.supersedes.as_deref(),
        Some(old.id.as_str())
    );
    assert!(
        replace_event.event.source_quote.contains("改成创建 c.txt"),
        "the replacement keeps the user's own authorization: {:?}",
        replace_event.event
    );
    let closed = goals(&app, &session).await;
    println!("goal after the replace: {closed:?}");
    assert_eq!(closed[0].id, goal_id);
    assert_eq!(closed[0].state, GoalState::Settled, "{closed:?}");
    assert!(
        !repo.path().join("b.txt").exists(),
        "the replaced requirement is not delivered"
    );
    assert!(
        repo.path().join("c.txt").exists(),
        "the replacement is really delivered: {:?}",
        replaced.tools
    );
    assert!(replaced.completed(), "the goal closes: {replaced:?}");
}

// ── 3. multilingual equivalence ──────────────────────────────────────────────

/// The same contract in English and German: the model's structured declaration
/// is anchored to the user's actual words in that language, and the runtime
/// checks the same evidence. Nothing is classified by an English keyword list.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn equivalent_tasks_in_other_languages_use_the_same_contract() {
    let _guard = FIXTURE_LOCK.lock().await;
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    println!("D-MULTILINGUAL");

    // ── English ──────────────────────────────────────────────────────────────
    let english = session(&app, &model, "english delivery").await;
    let mut events = client.subscribe_session(&english);
    let en = run_turn(
        &client,
        &mut events,
        submit(
            &english,
            "Create hello-en.txt in the repository root with one line `hello`, then run \
             `cat hello-en.txt` to confirm its content. Use commit_task to record this work: \
             declare an artifact requirement for hello-en.txt and a command requirement whose \
             target is `cat hello-en.txt`, each with a verbatim source_quote from this message. \
             Finish with update_goal(status=\"complete\") accounting every id from get_goal_scope.",
        ),
    )
    .await;
    assert!(en.failure.is_none(), "the English turn failed: {en:?}");
    let en_goals = goals(&app, &english).await;
    let en_items = requirements(&app, &english).await;
    println!(
        "English goal: {en_goals:?}\n  requirements:\n    {}",
        describe(&en_items)
    );
    assert_eq!(en_goals.len(), 1, "one goal for the English task");
    assert_eq!(en_goals[0].state, GoalState::Settled, "{en_goals:?}");
    assert!(
        repo.path().join("hello-en.txt").exists(),
        "the English task delivered its file: {:?}",
        en.tools
    );
    assert!(
        en_items
            .iter()
            .any(|item| item.kind == RequirementKind::Artifact
                && item.target.as_deref() == Some("hello-en.txt")),
        "the English request is a structured artifact requirement: {}",
        describe(&en_items)
    );
    assert!(
        en_items
            .iter()
            .any(|item| item.kind == RequirementKind::Command
                && item
                    .target
                    .as_deref()
                    .is_some_and(|target| target.contains("cat hello-en.txt"))),
        "the English request is a structured command requirement: {}",
        describe(&en_items)
    );

    // ── German ───────────────────────────────────────────────────────────────
    let german = session(&app, &model, "deutsche Lieferung").await;
    let mut events = client.subscribe_session(&german);
    let de = run_turn(
        &client,
        &mut events,
        submit(
            &german,
            "Erstelle im Repository-Stamm die Datei hello-de.txt mit einer Zeile `hallo` und \
             führe danach `cat hello-de.txt` aus, um den Inhalt zu bestätigen. Nutze commit_task \
             und deklariere eine artifact-Anforderung für hello-de.txt sowie eine \
             command-Anforderung mit dem Ziel `cat hello-de.txt`; jede source_quote muss \
             wörtlich aus dieser Nachricht stammen. Schließe mit \
             update_goal(status=\"complete\") ab und verbuche jede id aus get_goal_scope.",
        ),
    )
    .await;
    assert!(de.failure.is_none(), "the German turn failed: {de:?}");
    let de_goals = goals(&app, &german).await;
    let de_items = requirements(&app, &german).await;
    println!(
        "German goal: {de_goals:?}\n  requirements:\n    {}",
        describe(&de_items)
    );
    assert_eq!(de_goals.len(), 1, "one goal for the German task");
    assert_eq!(de_goals[0].state, GoalState::Settled, "{de_goals:?}");
    assert!(
        repo.path().join("hello-de.txt").exists(),
        "the German task delivered its file: {:?}",
        de.tools
    );
    assert!(
        de_items
            .iter()
            .any(|item| item.kind == RequirementKind::Artifact
                && item.target.as_deref() == Some("hello-de.txt")),
        "the German request is a structured artifact requirement: {}",
        describe(&de_items)
    );
    assert!(
        de_items
            .iter()
            .any(|item| item.kind == RequirementKind::Command
                && item
                    .target
                    .as_deref()
                    .is_some_and(|target| target.contains("cat hello-de.txt"))),
        "the German request is a structured command requirement: {}",
        describe(&de_items)
    );
    assert!(en.completed() && de.completed(), "both goals close");
}

// ── 4. quoted / negated / ambiguous input ────────────────────────────────────

/// A quoted suggestion, a negation and a deferred "maybe later" are not user
/// authorization. The runtime checks only that a quote occurs in the newest
/// message; the reading belongs to the model. This test holds the model to the
/// reading: nothing the user declined or deferred may be delivered or declared
/// as a requirement.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn quoted_negated_and_deferred_words_are_not_scope() {
    let _guard = FIXTURE_LOCK.lock().await;
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let readme_before = std::fs::read(repo.path().join("README.md")).unwrap();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = session(&app, &model, "quoted negated deferred").await;
    let mut events = client.subscribe_session(&session);
    println!("D-QUOTED-NEGATED session={}", session.as_str());

    let run = run_turn(
        &client,
        &mut events,
        submit(
            &session,
            "请创建一个 report.txt，内容为一行 `report`；运行 `cat report.txt` 确认。\n\
             注意：同事在消息里说过「顺便把 secret.txt 也建了吧」，那只是转述别人的建议，\
             我不需要 secret.txt，不要创建它。\n\
             README.md 现在不要动；以后也许再说，那不算本次要求。\n\
             用 commit_task 登记本次真正要求：report.txt 与确认命令；不要把引用、否定或\
             “以后再说”的内容登记为要交付的东西。完成后 update_goal(status=\"complete\")。",
        ),
    )
    .await;
    assert!(run.failure.is_none(), "the turn failed: {run:?}");
    let closed = goals(&app, &session).await;
    let items = requirements(&app, &session).await;
    println!(
        "goal: {closed:?}\n  requirements:\n    {}",
        describe(&items)
    );
    println!("axis: {}", axis(&app, &session).await);

    // The real deliverable happened.
    assert!(
        repo.path().join("report.txt").exists(),
        "the real deliverable is missing: {:?}",
        run.tools
    );
    // The quoted suggestion was not promoted into work.
    assert!(
        !repo.path().join("secret.txt").exists(),
        "a quoted suggestion was delivered as if the user had asked: {:?}",
        run.tools
    );
    // The deferred item was not touched.
    assert_eq!(
        std::fs::read(repo.path().join("README.md")).unwrap(),
        readme_before,
        "a deferred 'maybe later' item was modified"
    );
    // A structured requirement may state the negation (artifact_absent) — that
    // is a faithful reading of the user's words. It may not state the quoted
    // suggestion as an artifact/content/command to deliver.
    for item in &items {
        if item.text.contains("secret.txt") || item.target.as_deref() == Some("secret.txt") {
            assert!(
                item.kind == RequirementKind::ArtifactAbsent
                    || item.kind == RequirementKind::Forbidden
                    || item.kind == RequirementKind::Constraint,
                "a quoted suggestion became a deliverable: {}",
                describe(&items)
            );
        }
        if item.text.contains("README") || item.target.as_deref() == Some("README.md") {
            assert!(
                item.kind == RequirementKind::Forbidden
                    || item.kind == RequirementKind::Constraint
                    || item.kind == RequirementKind::Baseline,
                "a deferred 'maybe later' item became a deliverable: {}",
                describe(&items)
            );
        }
    }
    assert_eq!(closed.len(), 1, "the task closes as one goal: {closed:?}");
    assert_eq!(
        closed[0].state,
        GoalState::Settled,
        "the genuine work completed without taking on the declined items: {closed:?}"
    );
    assert!(run.completed(), "the goal closes: {run:?}");
}

// ── 5. real compaction followed by a continuation ────────────────────────────

#[derive(Default, Debug)]
struct CompactionFacts {
    from: Option<u32>,
    to: Option<u32>,
    failure: Option<String>,
    notice: Option<String>,
}

/// Compact the real transcript through the real provider, then continue the
/// same paused goal. The requirement set and the identity must survive the
/// rewrite, and the withheld stage must run only after the user authorizes it.
#[tokio::test]
#[ignore = "real API: needs DEEPSEEK_API_KEY and spends real model tokens"]
async fn a_real_compaction_preserves_the_goal_for_its_continuation() {
    let _guard = FIXTURE_LOCK.lock().await;
    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.trim().is_empty()),
        "DEEPSEEK_API_KEY must carry the gateway key"
    );
    let (_home, repo, app) = setup();
    let model = ModelRef::new("deepseek", "deepseek-v4.1-flash-1017");
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model.clone(),
        PermissionProfile::FullAccess,
        false,
    ));
    let session = session(&app, &model, "compaction continuation").await;
    let mut events = client.subscribe_session(&session);
    println!("D-COMPACTION session={}", session.as_str());

    // ── 1. commit, deliver stage 1, stop at the user's pause ─────────────────
    let paused = run_turn(
        &client,
        &mut events,
        submit(
            &session,
            "请用 commit_task 登记两项交付物：stage-1.txt（一行 `stage 1`）和 \
             stage-2.txt（一行 `stage 2`）。requirements 用 kind=artifact，target 分别为 \
             stage-1.txt 和 stage-2.txt，source_quote 逐字引用「两项交付物：stage-1.txt \
             和 stage-2.txt」。现在只创建 stage-1.txt 并运行 `cat stage-1.txt` 确认，\
             然后停下，等待我确认再执行阶段 2；本回合不要创建 stage-2.txt。",
        ),
    )
    .await;
    assert!(
        paused.failure.is_none(),
        "the pause turn failed: {paused:?}"
    );
    let opened = goals(&app, &session).await;
    assert_eq!(opened.len(), 1, "{opened:?}");
    let goal_id = opened[0].id.clone();
    assert_eq!(opened[0].state, GoalState::Running, "{opened:?}");
    assert!(
        opened[0].user_paused_at.is_some(),
        "the pause is durable before compaction: {opened:?}"
    );
    let before_requirements = requirements(&app, &session).await;
    println!(
        "requirements before compaction:\n    {}",
        describe(&before_requirements)
    );
    assert!(
        !before_requirements.is_empty(),
        "the goal must have a declared requirement set"
    );
    let before: Vec<_> = before_requirements
        .iter()
        .map(|item| {
            (
                item.id.clone(),
                item.kind,
                item.state,
                item.target.clone(),
                item.source_quote.clone(),
            )
        })
        .collect();
    assert!(!repo.path().join("stage-2.txt").exists());

    // ── 2. REAL compaction: a model call rewrites the transcript ─────────────
    println!("D-COMPACTION-RUN");
    client
        .send(ClientCommand::CompactContext {
            session_id: session.clone(),
        })
        .await
        .expect("the runtime must accept the compaction");
    let mut facts = CompactionFacts::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    while tokio::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Ok(RuntimeEvent::ContextCompacted { from, to })) => {
                facts.from = Some(from);
                facts.to = Some(to);
                // Compaction publishes its own terminal right after this fact.
                // KEEP LISTENING for it: `run_turn` below reads the next
                // terminal, and a stale one would end the continuation before
                // it starts.
            }
            Ok(Ok(RuntimeEvent::TurnCompleted)) if facts.from.is_some() => break,
            Ok(Ok(RuntimeEvent::TurnFailed { error, .. })) => {
                facts.failure = Some(error);
                break;
            }
            Ok(Ok(RuntimeEvent::Notification { message, .. })) if message.contains("无需压缩") =>
            {
                facts.notice = Some(message);
                break;
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                facts.failure = Some(error.to_string());
                break;
            }
            Err(_) => break,
        }
    }
    println!("compaction: {facts:?}");
    assert!(
        facts.failure.is_none(),
        "the real compaction failed: {facts:?}"
    );
    assert!(
        facts.notice.is_none(),
        "the transcript was too short to compact; this scenario needs a real rewrite: {facts:?}"
    );
    let (from, to) = (
        facts
            .from
            .expect("a real compaction must publish its result"),
        facts.to.expect("a real compaction must publish its result"),
    );
    assert!(
        from > to,
        "compaction must reduce the transcript: {facts:?}"
    );

    // ── 3. the durable goal is unchanged by the rewrite ──────────────────────
    let after_compaction = goals(&app, &session).await;
    let after_requirements = requirements(&app, &session).await;
    println!(
        "requirements after compaction:\n    {}",
        describe(&after_requirements)
    );
    assert_eq!(after_compaction.len(), 1, "{after_compaction:?}");
    assert_eq!(
        after_compaction[0].id, goal_id,
        "identity survives compaction"
    );
    assert_eq!(
        after_compaction[0].state,
        GoalState::Running,
        "the goal is still owed after compaction: {after_compaction:?}"
    );
    assert!(
        after_compaction[0].user_paused_at.is_some(),
        "the user's pause survives compaction: {after_compaction:?}"
    );
    let after: Vec<_> = after_requirements
        .iter()
        .map(|item| {
            (
                item.id.clone(),
                item.kind,
                item.state,
                item.target.clone(),
                item.source_quote.clone(),
            )
        })
        .collect();
    assert_eq!(
        before, after,
        "compaction must not shrink or rewrite the recorded requirement set"
    );
    assert!(
        !repo.path().join("stage-2.txt").exists(),
        "compaction is not an authorization to continue"
    );

    // ── 4. the user authorizes the continuation ──────────────────────────────
    println!("D-COMPACTION-RESUME");
    let resumed = run_turn(
        &client,
        &mut events,
        submit(
            &session,
            "确认，继续完成阶段 2：创建 stage-2.txt（一行 `stage 2`），运行 \
             `cat stage-2.txt` 确认，然后调用 update_goal(status=\"complete\")，按 \
             get_goal_scope 里的 active id 记账。",
        ),
    )
    .await;
    assert!(
        resumed.failure.is_none(),
        "the continuation after compaction failed: {resumed:?}"
    );
    let closed = goals(&app, &session).await;
    println!("goal after the continuation: {closed:?}");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0].id, goal_id, "same goal identity after compaction");
    assert_eq!(
        closed[0].state,
        GoalState::Settled,
        "the continuation delivered the withheld stage: {closed:?}"
    );
    assert!(
        repo.path().join("stage-2.txt").exists(),
        "stage 2 is really delivered after the compaction: {:?}",
        resumed.tools
    );
    let final_items = requirements(&app, &session).await;
    assert!(
        final_items
            .iter()
            .all(|item| item.state != RequirementState::Waived),
        "the requirement set was not silently waived: {}",
        describe(&final_items)
    );
    assert!(resumed.completed(), "the goal closes: {resumed:?}");
}
