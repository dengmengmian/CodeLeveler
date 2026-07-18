use leveler_execution::WorkspaceSnapshot;

fn git(root: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn snapshot_git_does_not_pass_credentials_to_clean_filter() {
    let dir = tempfile::tempdir().unwrap();
    let captured = dir.path().join("captured.txt");
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(dir.path().join("data.txt"), "value\n").unwrap();
    std::fs::write(dir.path().join(".gitattributes"), "*.txt filter=capture\n").unwrap();
    let filter = format!(
        "sh -c 'printf %s \"$LVTEST_SNAPSHOT_API_KEY\" > {}; cat'",
        captured.display()
    );
    git(dir.path(), &["config", "filter.capture.clean", &filter]);
    unsafe {
        std::env::set_var("LVTEST_SNAPSHOT_API_KEY", "must-not-leak");
    }

    WorkspaceSnapshot::capture(dir.path()).await.unwrap();

    assert_eq!(std::fs::read_to_string(&captured).unwrap(), "");
    unsafe {
        std::env::remove_var("LVTEST_SNAPSHOT_API_KEY");
    }
}
