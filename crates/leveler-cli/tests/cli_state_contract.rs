//! Stateful CLI surfaces as process behaviour.
//!
//! `sessions show|rename|delete`, `models show`, `background journal list`,
//! `schedule pause|resume`, `mcp add|list|remove` and the offline `eval trend`
//! projection were declared feature gaps even though the commands exist. These
//! tests drive the real binary against a real SQLite database and a real config
//! file, so the contract is what a person sees — not what an internal function
//! returns.
//!
//! The database is seeded through the production `SessionRepository`; it is a
//! fixture, not a mock. Nothing here reaches a provider or a network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn scratch(name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("lvl-state-{}-{n}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(path.join("ws/.leveler")).unwrap();
    path
}

fn leveler(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_leveler"))
        .args(args)
        .current_dir(home.join("ws"))
        .env("HOME", home)
        .env("LEVELER_HOME", home.join("leveler"))
        .env("LEVELER_CONFIG", home.join("leveler/config.toml"))
        .env("LEVELER_LANG", "zh")
        .env("NO_COLOR", "1")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("LEVELER_API_KEY")
        .output()
        .unwrap_or_else(|err| panic!("could not run leveler {args:?}: {err}"))
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Persist one session exactly where the CLI resolves its database.
async fn seed_session(home: &Path, goal: &str) -> String {
    use leveler_storage::{Database, SessionRecord, SessionRepository};

    let workspace = std::fs::canonicalize(home.join("ws")).unwrap();
    let leveler_home = leveler_core::LevelerHome::from_root(home.join("leveler"));
    let state_dir =
        leveler_home.project_state_dir(&leveler_project::layout::encode_repo_path(&workspace));
    std::fs::create_dir_all(&state_dir).unwrap();
    let db = Database::connect(&state_dir.join("sessions.db"))
        .await
        .unwrap();
    let record = SessionRecord::new(
        workspace.to_string_lossy(),
        goal,
        "deepseek/deepseek-flash",
        leveler_core::now(),
    );
    let id = record.id.clone();
    SessionRepository::new(&db).create(&record).await.unwrap();
    id
}

fn minimal_config(home: &Path) {
    std::fs::create_dir_all(home.join("leveler")).unwrap();
    std::fs::write(
        home.join("leveler/config.toml"),
        r#"default_model = "deepseek/deepseek-flash"

[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek-flash"]
provider = "deepseek"
context_window = 131072
max_output_tokens = 16384
"#,
    )
    .unwrap();
}

#[tokio::test]
async fn sessions_show_reports_a_persisted_session_and_refuses_an_unknown_id() {
    let home = scratch("show");
    let id = seed_session(&home, "修复登录 bug").await;

    let human = leveler(&home, &["sessions", "show", &id]);
    let rendered = text(&human);
    assert!(human.status.success(), "{rendered}");
    assert!(rendered.contains(&id), "the id is printed: {rendered}");
    assert!(
        rendered.contains("修复登录 bug"),
        "the original request is readable: {rendered}"
    );

    let json = leveler(&home, &["sessions", "show", &id, "--json"]);
    let rendered = text(&json);
    assert!(json.status.success(), "{rendered}");
    let parsed: serde_json::Value =
        serde_json::from_str(rendered.trim()).expect("show --json is one JSON document");
    assert_eq!(parsed["id"], id);
    assert_eq!(parsed["goal"], "修复登录 bug");
    assert_eq!(parsed["model"], "deepseek/deepseek-flash");

    let missing = leveler(&home, &["sessions", "show", "does-not-exist"]);
    assert_eq!(
        missing.status.code(),
        Some(1),
        "an unknown id is a reported refusal: {}",
        text(&missing)
    );
    assert!(
        text(&missing).contains("does-not-exist"),
        "the refusal names the id: {}",
        text(&missing)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn sessions_rename_changes_only_the_display_title() {
    let home = scratch("rename");
    let id = seed_session(&home, "original request").await;

    let renamed = leveler(&home, &["sessions", "rename", &id, "  new title  "]);
    let rendered = text(&renamed);
    assert!(renamed.status.success(), "{rendered}");
    assert!(rendered.contains("new title"), "{rendered}");

    let json = leveler(&home, &["sessions", "show", &id, "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(text(&json).trim()).unwrap();
    assert_eq!(
        parsed["title"], "new title",
        "the title is trimmed and stored"
    );
    assert_eq!(
        parsed["goal"], "original request",
        "rename never rewrites what the user asked for"
    );

    let empty = leveler(&home, &["sessions", "rename", &id, "   "]);
    assert_eq!(
        empty.status.code(),
        Some(1),
        "an empty title is refused: {}",
        text(&empty)
    );
    let after: serde_json::Value =
        serde_json::from_str(text(&leveler(&home, &["sessions", "show", &id, "--json"])).trim())
            .unwrap();
    assert_eq!(after["title"], "new title", "a refusal changes nothing");
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn sessions_delete_removes_the_session_and_refuses_a_missing_id() {
    let home = scratch("delete");
    let id = seed_session(&home, "delete me").await;

    let deleted = leveler(&home, &["sessions", "delete", &id]);
    let rendered = text(&deleted);
    assert!(deleted.status.success(), "{rendered}");
    assert!(rendered.contains(&id), "{rendered}");

    let show = leveler(&home, &["sessions", "show", &id]);
    assert_eq!(
        show.status.code(),
        Some(1),
        "a deleted session is gone: {}",
        text(&show)
    );
    assert!(
        text(&show).contains("No session"),
        "the refusal says the row is gone: {}",
        text(&show)
    );

    let again = leveler(&home, &["sessions", "delete", &id]);
    assert_eq!(
        again.status.code(),
        Some(1),
        "deleting twice refuses rather than reporting success: {}",
        text(&again)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn models_show_prints_the_configured_profile_and_refuses_an_unknown_model() {
    let home = scratch("model-show");
    minimal_config(&home);

    let shown = leveler(&home, &["models", "show", "deepseek/deepseek-flash"]);
    let rendered = text(&shown);
    assert!(shown.status.success(), "{rendered}");
    assert!(
        rendered.contains("deepseek/deepseek-flash"),
        "the resolved reference is the heading: {rendered}"
    );
    assert!(rendered.contains("model_id"), "{rendered}");
    assert!(rendered.contains("context"), "{rendered}");

    let unknown = leveler(&home, &["models", "show", "deepseek/does-not-exist"]);
    let rendered = text(&unknown);
    assert!(
        !unknown.status.success(),
        "an unknown model is a refusal, not a silent default: {rendered}"
    );
    assert!(!rendered.contains("panicked"), "{rendered}");
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn schedule_pause_and_resume_toggle_the_definition_and_refuse_a_missing_id() {
    let home = scratch("schedule");
    let workspace = home.join("ws");
    let added = leveler(
        &home,
        &[
            "schedule",
            "add",
            "--name",
            "resume contract",
            "--workspace",
            workspace.to_str().unwrap(),
            "--command",
            "true",
            "--at",
            "2030-01-01T00:00:00Z",
            "--json",
        ],
    );
    let rendered = text(&added);
    assert!(added.status.success(), "{rendered}");
    let definition: serde_json::Value =
        serde_json::from_str(rendered.trim()).expect("schedule add --json is one document");
    let id = definition["id"]
        .as_str()
        .expect("the definition carries its id");

    let enabled = |home: &Path| -> bool {
        let listed = leveler(home, &["schedule", "list", "--json"]);
        let rendered = text(&listed);
        assert!(listed.status.success(), "{rendered}");
        let rows: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .expect("the definition is listed")["enabled"]
            .as_bool()
            .unwrap()
    };
    assert!(enabled(&home), "a new definition starts enabled");

    let paused = leveler(&home, &["schedule", "pause", id]);
    assert!(paused.status.success(), "{}", text(&paused));
    assert!(!enabled(&home), "pause persists a disabled definition");

    let resumed = leveler(&home, &["schedule", "resume", id]);
    assert!(resumed.status.success(), "{}", text(&resumed));
    assert!(enabled(&home), "resume re-enables the same definition");

    let missing = leveler(&home, &["schedule", "resume", "sched-missing"]);
    assert_eq!(
        missing.status.code(),
        Some(1),
        "resuming an unknown schedule is a reported refusal: {}",
        text(&missing)
    );
    assert!(
        text(&missing).contains("sched-missing"),
        "the refusal names the id: {}",
        text(&missing)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn mcp_add_list_and_remove_round_trip_through_the_config_file() {
    let home = scratch("mcp");
    let server_command = std::env::current_exe()
        .expect("the test binary has an absolute path")
        .to_string_lossy()
        .into_owned();

    let empty = leveler(&home, &["mcp", "list"]);
    assert!(empty.status.success(), "{}", text(&empty));
    assert!(
        text(&empty).contains("No MCP servers configured"),
        "an empty listing says so: {}",
        text(&empty)
    );

    let added = leveler(
        &home,
        &["mcp", "add", "demo", "--", &server_command, "hello"],
    );
    assert!(added.status.success(), "{}", text(&added));
    let config = std::fs::read_to_string(home.join("leveler/config.toml")).unwrap();
    assert!(
        config.contains("demo"),
        "the server name reaches the config"
    );
    assert!(
        config.contains(&server_command),
        "the launch command reaches the config"
    );

    let listed = leveler(&home, &["mcp", "list"]);
    let rendered = text(&listed);
    assert!(listed.status.success(), "{rendered}");
    assert!(rendered.contains("demo"), "{rendered}");
    assert!(rendered.contains(&server_command), "{rendered}");

    let removed = leveler(&home, &["mcp", "remove", "demo"]);
    assert!(removed.status.success(), "{}", text(&removed));
    let empty_again = leveler(&home, &["mcp", "list"]);
    assert!(
        text(&empty_again).contains("No MCP servers configured"),
        "remove deletes the entry: {}",
        text(&empty_again)
    );

    let missing = leveler(&home, &["mcp", "remove", "demo"]);
    assert_eq!(
        missing.status.code(),
        Some(1),
        "removing an unknown server is a reported refusal: {}",
        text(&missing)
    );
    assert!(
        text(&missing).contains("no MCP server named"),
        "the refusal explains why: {}",
        text(&missing)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn eval_trend_reads_baseline_artifacts_and_reports_a_regression() {
    let home = scratch("trend");
    let history = home.join("history");
    std::fs::create_dir_all(&history).unwrap();
    for (version, score, completion, false_rate) in
        [("1.0.27", 92, 1.0, 0.0), ("1.0.28", 88, 0.9, 0.1)]
    {
        let document = serde_json::json!({
            "kind": "run",
            "meta": {
                "created_at": "2026-11-01T00:00:00Z",
                "cases_dir": "cases",
                "mode": "direct",
                "repetitions": 1,
                "model_refs": ["deepseek/deepseek-flash"],
                "engine_version": version,
            },
            "report": {
                "model": "deepseek/deepseek-flash",
                "cases": [{
                    "id": "one",
                    "completed": true,
                    "expect_passed": true,
                    "rounds": 3,
                    "note": "ok",
                }],
            },
            "completion_rate": completion,
            "passed": 1,
            "total": 1,
            "failed_case_ids": [],
            "false_completion_rate": false_rate,
            "quality_score_100": score,
        });
        std::fs::write(
            history.join(format!("{version}.json")),
            serde_json::to_vec_pretty(&document).unwrap(),
        )
        .unwrap();
    }

    let report = home.join("trend.md");
    let rendered = leveler(
        &home,
        &[
            "eval",
            "trend",
            "--history",
            history.to_str().unwrap(),
            "--out",
            report.to_str().unwrap(),
        ],
    );
    assert!(rendered.status.success(), "{}", text(&rendered));
    let markdown = std::fs::read_to_string(&report).unwrap();
    assert!(markdown.contains("1.0.27"), "{markdown}");
    assert!(markdown.contains("1.0.28"), "{markdown}");
    assert!(
        markdown.contains("**1.0.27 → 1.0.28**: -4 points"),
        "a real score drop is reported, not smoothed away: {markdown}"
    );

    let empty = home.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let refused = leveler(
        &home,
        &["eval", "trend", "--history", empty.to_str().unwrap()],
    );
    assert_eq!(
        refused.status.code(),
        Some(1),
        "an empty history is a refusal: {}",
        text(&refused)
    );
    assert!(
        text(&refused).contains("no run baselines"),
        "the refusal explains what is missing: {}",
        text(&refused)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn background_journal_list_reports_no_pending_reservations_on_a_fresh_home() {
    let home = scratch("journal");
    let listed = leveler(&home, &["background", "journal", "list"]);
    let rendered = text(&listed);
    assert!(listed.status.success(), "{rendered}");
    assert!(
        !rendered.trim().is_empty(),
        "an empty result still says something: {rendered}"
    );

    let json = leveler(&home, &["background", "journal", "list", "--json"]);
    let rendered = text(&json);
    assert!(json.status.success(), "{rendered}");
    let parsed: serde_json::Value =
        serde_json::from_str(rendered.trim()).expect("journal list --json is one JSON document");
    assert_eq!(parsed, serde_json::json!([]));
    let _ = std::fs::remove_dir_all(&home);
}
