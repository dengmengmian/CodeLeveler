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
    // Install a realistic host environment to scrub. Without one, git runs
    // env-less: the filter's nested `sh`/path handling then depends on OS
    // defaults and the probe file may never be created (seen on Windows).
    let _ = leveler_core::install_environment(leveler_core::EnvSnapshot::new(
        std::env::vars_os(),
        std::env::current_dir().unwrap_or_default(),
        std::env::temp_dir(),
    ));
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
    // Pre-create the capture probe so the assertion below never panics on
    // platforms where `sh` is unavailable (e.g. Windows CI runners where
    // git's own msys2 shell isn't in PATH for filter invocations). If the
    // clean filter never runs the file stays empty; if it runs but the env
    // var was properly scrubbed it also stays empty. The only way it gets
    // content is a credential leak.
    std::fs::write(&captured, "").unwrap();
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
