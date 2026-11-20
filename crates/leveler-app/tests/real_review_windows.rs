//! Real-model fixed-commit review on Windows.
//!
//! Ignored by default because it spends real tokens and needs the configured
//! gateway. It drives the production in-process runtime with the real
//! provider; the reviewer's only object source is `git_history`, which on
//! Windows reads the local object database without starting `git`.
//!
//! ```text
//! LEVELER_HOME=C:\path\to\home cargo test -p leveler-app \
//!   --test real_review_windows -- --ignored --nocapture
//! ```

#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, InteractiveRuntimeClient, RuntimeEvent, UiReviewScope, UiReviewStatus,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_storage::ReviewRepository;

const TIMEOUT: Duration = Duration::from_secs(900);

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real API: spends real model tokens and needs the configured provider"]
async fn a_real_reviewer_reads_a_fixed_commit_through_the_local_object_reader() {
    let home =
        std::env::var_os("LEVELER_HOME").expect("LEVELER_HOME must point at a configured home");
    unsafe {
        std::env::set_var("LEVELER_HOME", &home);
    }
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "qa@example.test"]);
    git(repo.path(), &["config", "user.name", "QA"]);
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/billing.rs"),
        "pub fn load(document_id: &str) -> String {\n    \
         // BUG: no tenant/owner check before reading the document\n    \
         format!(\"document:{document_id}\")\n}\n",
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "introduce billing read"]);
    let reviewed = git(repo.path(), &["rev-parse", "HEAD"]);
    std::fs::write(
        repo.path().join("src/billing.rs"),
        "pub fn load(owner: &str, document_id: &str) -> String {\n    \
         // FIX (later commit): the caller must present the owner\n    \
         format!(\"document:{owner}:{document_id}\")\n}\n",
    )
    .unwrap();
    git(repo.path(), &["commit", "-qam", "fix: require owner"]);

    let layout = Layout::from_parts(
        repo.path().to_path_buf(),
        repo.path().join("configs"),
        repo.path().join("state"),
    );
    let app = Arc::new(
        Application::assemble(layout)
            .unwrap()
            .with_collaboration(leveler_agent::CollaborationMode::Chat),
    );
    let model = ModelRef::new("taotoken", "deepseek-v4.1-flash-1017");
    let session = app
        .create_session(&model, "审查固定提交的账单读取")
        .await
        .unwrap();
    let client = Arc::new(InProcessRuntimeClient::new(
        app.clone(),
        model,
        PermissionProfile::Assisted,
        false,
    ));
    let mut events = client.subscribe_session(&session);
    client
        .send(ClientCommand::RunReview {
            session_id: session.clone(),
            focus: "检查该提交的账单读取实现是否缺少 owner/租户校验".into(),
            scope: UiReviewScope::Commit {
                rev: reviewed.clone(),
            },
            model: None,
        })
        .await
        .unwrap();

    let summary = tokio::time::timeout(TIMEOUT, async {
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::ReviewFinished { review, .. }) => return review,
                Ok(RuntimeEvent::Notification { message, .. }) => {
                    eprintln!("notification: {message}");
                }
                Ok(_) => continue,
                Err(error) => panic!("event stream closed: {error}"),
            }
        }
    })
    .await
    .expect("the real review must settle");
    assert_eq!(summary.status, UiReviewStatus::Completed, "{summary:?}");

    let db = app.open_database().await.unwrap();
    let stored = ReviewRepository::new(&db)
        .get(&summary.id)
        .await
        .unwrap()
        .expect("the review row is durable");
    let activity = stored.activity_json.clone();
    assert!(
        activity.contains("git_history"),
        "the reviewer must have used the revision-scoped tool: {activity}"
    );
    let evidence = std::env::var_os("LEVELER_REVIEW_EVIDENCE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&evidence).unwrap();
    std::fs::write(
        evidence.join("real-review-summary.json"),
        serde_json::json!({
            "reviewed_commit": reviewed,
            "status": format!("{:?}", summary.status),
            "id": summary.id.to_string(),
            "counts": serde_json::to_value(summary.counts).unwrap_or_default(),
            "report": stored.report,
            "report_source": stored.report_source,
            "activity": activity,
        })
        .to_string(),
    )
    .unwrap();
    eprintln!("real review completed: {}", summary.id);
}
