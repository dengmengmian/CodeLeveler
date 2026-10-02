//! Phase 3C: grant-eligible Git commands must execute their approved remote
//! target even when the repository configuration, the remote name, or the
//! environment changes after admission. Every case is exercised against a real
//! (dumb-HTTP) Git server that records the paths it is actually asked for, so
//! "the server never saw the other repository" is observed, not asserted about
//! an internal flag.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use leveler_execution::{approved_git_target, isolate_git_command};

/// A real HTTP server that serves a directory of bare repositories and records
/// every request path. Dumb HTTP is enough to prove *which* repository Git
/// contacted: `git update-server-info` publishes the refs and objects Git then
/// fetches.
struct RecordedHttp {
    base: String,
    log: Arc<Mutex<Vec<String>>>,
}

fn serve(root: PathBuf) -> RecordedHttp {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorded = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = root.clone();
            let recorded = recorded.clone();
            std::thread::spawn(move || handle(stream, &root, &recorded));
        }
    });
    RecordedHttp {
        base: format!("http://{address}"),
        log,
    }
}

fn handle(mut stream: std::net::TcpStream, root: &Path, log: &Arc<Mutex<Vec<String>>>) {
    let mut buffer = [0u8; 4096];
    let Ok(read) = stream.read(&mut buffer) else {
        return;
    };
    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
    let Some(line) = request.lines().next() else {
        return;
    };
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    log.lock().unwrap().push(path.clone());
    let file = path
        .split('?')
        .next()
        .unwrap_or("/")
        .trim_start_matches('/');
    let body = std::fs::read(root.join(file)).ok();
    let response = match body {
        Some(body) => {
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            response
        }
        None => {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
    };
    let _ = stream.write_all(&response);
}

fn git(cwd: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git fixture step failed: {args:?}");
}

/// A published bare repository under `served`, plus a consumer checkout that
/// tracks it through the recorded server.
fn grant_for(repo: &Path, args: &[&str]) -> leveler_core::GrantRequest {
    let owned = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    resolve_git_grant_blocking("git", &owned, repo)
        .expect("fixture Git command must be grant-eligible")
        .expect("fixture remote must be attributable")
}

fn resolve_git_grant_blocking(
    program: &str,
    args: &[String],
    cwd: &Path,
) -> Result<Option<leveler_core::GrantRequest>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(leveler_execution::resolve_git_grant(program, args, cwd))
}

fn run_isolated(isolation: &leveler_execution::GitIsolation, cwd: &Path) -> std::process::Output {
    let mut command = std::process::Command::new(isolation.program());
    command
        .args(isolation.args())
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default());
    for (name, value) in isolation.authority_env() {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn isolate(
    repo: &Path,
    grant: &leveler_core::GrantRequest,
    args: &[&str],
) -> Result<leveler_execution::GitIsolation, String> {
    let owned = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let target = approved_git_target(grant, "git", &owned)
        .map_err(|error| error.to_string())?
        .expect("grant must produce an approved target");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(isolate_git_command(
        &target,
        &owned,
        repo,
        leveler_core::environment(),
    ))
}

fn recorded_paths(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// Baseline: the frozen invocation reaches the approved repository and never
/// touches any other one.
#[test]
fn frozen_fetch_contacts_only_the_approved_repository() {
    let served = tempfile::tempdir().unwrap();
    let server = serve(served.path().to_path_buf());
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    let grant = grant_for(&consumer, &["fetch", "origin"]);
    let isolation = isolate(&consumer, &grant, &["fetch", "origin"]).unwrap();
    let output = run_isolated(&isolation, &consumer);
    assert!(
        output.status.success(),
        "frozen fetch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let paths = recorded_paths(&server.log);
    assert!(!paths.is_empty(), "the server must have been contacted");
    assert!(
        paths.iter().all(|path| path.starts_with("/approved.git/")),
        "unexpected request paths: {paths:?}"
    );
}

/// Case A — the remote URL changes after approval: no request goes to the new
/// destination.
#[test]
fn mutated_remote_url_never_reaches_the_new_destination() {
    let served = tempfile::tempdir().unwrap();
    let server = serve(served.path().to_path_buf());
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    let grant = grant_for(&consumer, &["fetch", "origin"]);
    // Publish a second repository the attacker tries to redirect to.
    publish_bare(served.path(), "changed.git");
    git(
        &consumer,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{}/changed.git", server.base),
        ],
    );
    assert!(
        isolate(&consumer, &grant, &["fetch", "origin"]).is_err(),
        "a changed remote URL must fail closed"
    );
    let paths = recorded_paths(&server.log);
    assert!(
        paths.iter().all(|path| path.starts_with("/approved.git/")),
        "changed.git was contacted: {paths:?}"
    );
}

/// Case B — a `pushurl` added after approval cannot move the push target.
#[test]
fn mutated_pushurl_never_reaches_the_new_destination() {
    let served = tempfile::tempdir().unwrap();
    let server = serve(served.path().to_path_buf());
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    let grant = grant_for(&consumer, &["push", "origin", "main"]);
    publish_bare(served.path(), "changed.git");
    git(
        &consumer,
        &[
            "config",
            "remote.origin.pushurl",
            &format!("{}/changed.git", server.base),
        ],
    );
    assert!(
        isolate(&consumer, &grant, &["push", "origin", "main"]).is_err(),
        "a changed pushurl must fail closed"
    );
    let paths = recorded_paths(&server.log);
    assert!(
        paths.iter().all(|path| path.starts_with("/approved.git/")),
        "changed.git was contacted: {paths:?}"
    );
}

/// Case C — an `insteadOf` rewrite added after approval is refused.
#[test]
fn instead_of_rewrite_is_refused() {
    let served = tempfile::tempdir().unwrap();
    let server = serve(served.path().to_path_buf());
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    let grant = grant_for(&consumer, &["fetch", "origin"]);
    git(
        &consumer,
        &[
            "config",
            &format!("url.{}/changed.git.insteadOf", server.base),
            &format!("{}/approved.git", server.base),
        ],
    );
    assert!(
        isolate(&consumer, &grant, &["fetch", "origin"]).is_err(),
        "an URL rewrite must fail closed"
    );
    assert!(recorded_paths(&server.log).is_empty());
}

/// Case D — a repository-local credential helper added after approval cannot
/// run: the frozen invocation resets the helper list, so only an approved,
/// execution-bound broker helper is ever active. Executable transport
/// overrides (`core.sshCommand`) remain an effect-critical refusal.
#[test]
fn helper_is_neutralized_and_ssh_overrides_are_refused() {
    let served = tempfile::tempdir().unwrap();
    let server = serve(served.path().to_path_buf());
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    let grant = grant_for(&consumer, &["fetch", "origin"]);
    git(
        &consumer,
        &["config", "credential.helper", "!attacker-helper"],
    );
    let isolation = isolate(&consumer, &grant, &["fetch", "origin"])
        .expect("a repository credential helper must be neutralized, not inherited");
    assert!(
        !isolation
            .args()
            .iter()
            .any(|arg| arg.contains("attacker-helper")),
        "the repository's credential helper leaked into the frozen invocation: {:?}",
        isolation.args()
    );
    let output = run_isolated(&isolation, &consumer);
    assert!(
        output.status.success(),
        "a neutralized repository helper must not block a public fetch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        recorded_paths(&server.log)
            .iter()
            .all(|path| path.starts_with("/approved.git/")),
        "a non-approved repository was contacted"
    );
    git(&consumer, &["config", "--unset", "credential.helper"]);
    git(
        &consumer,
        &["config", "core.sshCommand", "/tmp/attacker-ssh"],
    );
    assert!(isolate(&consumer, &grant, &["fetch", "origin"]).is_err());
}

/// Case E — a replaced repository invalidates the approved identity.
#[test]
fn replaced_repository_fails_closed() {
    let served = tempfile::tempdir().unwrap();
    let server = serve(served.path().to_path_buf());
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    let grant = grant_for(&consumer, &["fetch", "origin"]);
    std::fs::rename(consumer.join(".git"), consumer.join("old-git")).unwrap();
    git(&consumer, &["init", "-q"]);
    assert!(
        isolate(&consumer, &grant, &["fetch", "origin"]).is_err(),
        "a replaced repository must fail closed"
    );
    assert!(recorded_paths(&server.log).is_empty());
}

fn publish_bare(served: &Path, name: &str) {
    let bare = served.join(name);
    git(served, &["init", "-q", "--bare", name]);
    git(
        served,
        &["--git-dir", bare.to_str().unwrap(), "update-server-info"],
    );
}

fn fixtures_with_base(served: &Path, base: &str) -> (PathBuf, PathBuf) {
    let approved = served.join("approved.git");
    let origin = served.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q"]);
    git(&origin, &["config", "user.email", "fixture@example.com"]);
    git(&origin, &["config", "user.name", "fixture"]);
    std::fs::write(origin.join("README.md"), "approved\n").unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-qm", "init"]);
    git(served, &["init", "-q", "--bare", "approved.git"]);
    git(
        served,
        &[
            "--git-dir",
            approved.to_str().unwrap(),
            "fetch",
            origin.to_str().unwrap(),
            "HEAD:refs/heads/main",
        ],
    );
    git(
        served,
        &[
            "--git-dir",
            approved.to_str().unwrap(),
            "update-server-info",
        ],
    );

    let consumer = served.join("consumer");
    std::fs::create_dir_all(&consumer).unwrap();
    git(&consumer, &["init", "-q"]);
    git(&consumer, &["config", "user.email", "fixture@example.com"]);
    git(&consumer, &["config", "user.name", "fixture"]);
    git(&consumer, &["config", "credential.helper", ""]);
    git(
        &consumer,
        &["remote", "add", "origin", &format!("{base}/approved.git")],
    );
    (consumer, approved)
}
