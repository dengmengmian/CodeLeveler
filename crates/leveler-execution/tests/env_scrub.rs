//! Subprocess environment scrubbing: provider credentials and other secrets
//! must never leak into `run_command` children. Lives in an integration test
//! (not the lib's test module) because setting parent env vars needs `unsafe`
//! in edition 2024 and the lib forbids unsafe code.

use leveler_execution::{CommandRunner, ProcessRequest};

fn runner() -> CommandRunner {
    CommandRunner::with_environment(std::sync::Arc::new(leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap(),
        std::env::temp_dir(),
    )))
}
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn secret_suffixed_env_vars_are_scrubbed() {
    // Set in the parent; the child must not inherit them.
    unsafe {
        std::env::set_var("LVTEST_MYPROV_API_KEY", "k");
        std::env::set_var("LVTEST_AUTH_TOKEN", "t");
        std::env::set_var("LVTEST_CLIENT_SECRET", "s");
        std::env::set_var("LVTEST_DB_PASSWORD", "p");
        std::env::set_var("LVTEST_PLAIN", "visible");
    }
    let out = runner()
        .run(
            ProcessRequest::new(
                "sh",
                vec![
                    "-c".into(),
                    "echo \"${LVTEST_MYPROV_API_KEY:-}|${LVTEST_AUTH_TOKEN:-}|${LVTEST_CLIENT_SECRET:-}|${LVTEST_DB_PASSWORD:-}|${LVTEST_PLAIN:-}\"".into(),
                ],
                std::env::temp_dir(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        out.stdout.trim(),
        "||||visible",
        "secret-suffixed vars must be scrubbed, ordinary vars must pass through"
    );
}

#[tokio::test]
async fn deny_env_entries_are_scrubbed() {
    unsafe {
        std::env::set_var("LVTEST_CUSTOM_CREDENTIAL", "leak");
    }
    let mut req = ProcessRequest::new(
        "sh",
        vec!["-c".into(), "echo \"${LVTEST_CUSTOM_CREDENTIAL:-}\"".into()],
        std::env::temp_dir(),
    );
    req.deny_env = vec!["LVTEST_CUSTOM_CREDENTIAL".to_string()];
    let out = runner().run(req, CancellationToken::new()).await.unwrap();
    assert_eq!(out.stdout.trim(), "", "deny_env names must be scrubbed");
}

#[tokio::test]
async fn builtin_denylist_is_scrubbed() {
    unsafe {
        std::env::set_var("DEEPSEEK_API_KEY", "sk-leak");
    }
    let out = runner()
        .run(
            ProcessRequest::new(
                "sh",
                vec!["-c".into(), "echo \"${DEEPSEEK_API_KEY:-}\"".into()],
                std::env::temp_dir(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.stdout.trim(), "");
}

#[tokio::test]
async fn credential_requires_an_explicit_child_grant() {
    unsafe {
        std::env::set_var("LVTEST_EXPLICIT_TOKEN", "authorized");
    }
    let mut request = ProcessRequest::new(
        "sh",
        vec!["-c".into(), "printf %s \"$LVTEST_EXPLICIT_TOKEN\"".into()],
        std::env::temp_dir(),
    );
    request.allow_env = vec!["LVTEST_EXPLICIT_TOKEN".to_string()];

    let output = runner()
        .run(request, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(output.stdout, "authorized");
}

#[tokio::test]
async fn credential_added_after_snapshot_is_not_inherited() {
    let runner = runner();
    unsafe { std::env::set_var("LVTEST_LATE_API_KEY", "late-secret") };
    let out = runner
        .run(
            ProcessRequest::new(
                "sh",
                vec![
                    "-c".into(),
                    "printf %s \"${LVTEST_LATE_API_KEY-unset}\"".into(),
                ],
                std::env::temp_dir(),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    unsafe { std::env::remove_var("LVTEST_LATE_API_KEY") };

    assert_eq!(
        out.stdout, "unset",
        "live parent env must never bypass the snapshot"
    );
}
