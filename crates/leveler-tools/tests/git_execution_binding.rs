//! Phase 3C wiring: an admitted Git resource grant must reach the command
//! execution layer and run against the frozen approved target. The execution
//! binding is proven against a real (dumb-HTTP) server that records the paths
//! it is asked for, so the repository actually contacted is observed.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use leveler_core::GrantScope;
use leveler_execution::{
    AuthorizationEvidence, NetworkScope, PermissionProfile, ResolvedExecutionPolicy, Workspace,
    WriteScope,
};
use leveler_tools::ToolContext;
use leveler_tools::default_registry;
use tokio_util::sync::CancellationToken;

fn serve(root: std::path::PathBuf) -> (String, Arc<Mutex<Vec<String>>>) {
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
    (format!("http://{address}"), log)
}

fn handle(mut stream: TcpStream, root: &Path, log: &Arc<Mutex<Vec<String>>>) {
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
    match std::fs::read(root.join(file)) {
        Ok(body) => {
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            let _ = stream.write_all(&response);
        }
        Err(_) => {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    }
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

fn fixture(served: &Path, base: &str) -> std::path::PathBuf {
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
    consumer
}

async fn run(ctx: ToolContext, args: serde_json::Value) -> Result<(bool, String), String> {
    default_registry()
        .execute("run_command", args, ctx, CancellationToken::new())
        .await
        .map(|output| (output.is_error, output.content))
        .map_err(|error| error.to_string())
}

/// The command sandbox needs a resolved, workspace-external Leveler home; a
/// test binary has no configured home, so state one explicitly.
fn test_environment() -> Arc<leveler_core::EnvSnapshot> {
    let base = leveler_core::environment();
    let home = std::env::temp_dir().join(format!("leveler-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = base
        .vars_os()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    vars.push((
        std::ffi::OsString::from("LEVELER_HOME"),
        home.as_os_str().to_os_string(),
    ));
    Arc::new(leveler_core::EnvSnapshot::new(
        vars,
        base.current_dir().to_path_buf(),
        base.temp_dir().to_path_buf(),
    ))
}

/// An approved Git grant reaches the execution layer and the command contacts
/// only the approved repository.
#[tokio::test]
async fn approved_git_grant_executes_against_the_frozen_target() {
    let served = tempfile::tempdir().unwrap();
    let (base, log) = serve(served.path().to_path_buf());
    let consumer = fixture(served.path(), &base);
    let grant =
        leveler_execution::resolve_git_grant("git", &["fetch".into(), "origin".into()], &consumer)
            .await
            .unwrap()
            .unwrap();
    let workspace = Workspace::new(&consumer).unwrap();
    let root = workspace.root().to_path_buf();
    let policy = ResolvedExecutionPolicy::new(
        WriteScope::WorkspaceWithGit { root },
        NetworkScope::Internet,
        AuthorizationEvidence::ResourceGrant {
            request: grant,
            scope: GrantScope::Once,
        },
    );
    let ctx =
        ToolContext::with_environment(workspace, PermissionProfile::Assisted, test_environment())
            .with_resolved_policy(policy);
    let (is_error, content) = run(
        ctx,
        serde_json::json!({"program":"git","args":["fetch","origin"]}),
    )
    .await
    .expect("the frozen fetch must execute");
    assert!(!is_error, "frozen fetch failed: {content}");
    let paths = log.lock().unwrap().clone();
    assert!(!paths.is_empty(), "the server must have been contacted");
    assert!(
        paths.iter().all(|path| path.starts_with("/approved.git/")),
        "unexpected request paths: {paths:?}"
    );
}

/// A changed remote URL invalidates the grant at execution: the command fails
/// and the changed destination is never contacted.
#[tokio::test]
async fn mutated_remote_is_refused_before_any_request() {
    let served = tempfile::tempdir().unwrap();
    let (base, log) = serve(served.path().to_path_buf());
    let consumer = fixture(served.path(), &base);
    let grant =
        leveler_execution::resolve_git_grant("git", &["fetch".into(), "origin".into()], &consumer)
            .await
            .unwrap()
            .unwrap();
    git(served.path(), &["init", "-q", "--bare", "changed.git"]);
    git(
        &consumer,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{base}/changed.git"),
        ],
    );
    let workspace = Workspace::new(&consumer).unwrap();
    let root = workspace.root().to_path_buf();
    let policy = ResolvedExecutionPolicy::new(
        WriteScope::WorkspaceWithGit { root },
        NetworkScope::Internet,
        AuthorizationEvidence::ResourceGrant {
            request: grant,
            scope: GrantScope::Once,
        },
    );
    let ctx =
        ToolContext::with_environment(workspace, PermissionProfile::Assisted, test_environment())
            .with_resolved_policy(policy);
    match run(
        ctx,
        serde_json::json!({"program":"git","args":["fetch","origin"]}),
    )
    .await
    {
        Err(reason) => assert!(
            reason.contains("resource binding refused"),
            "a changed remote must be refused by the binding: {reason}"
        ),
        Ok((is_error, content)) => {
            assert!(is_error, "a changed remote must fail: {content}")
        }
    }
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .all(|path| path.starts_with("/approved.git/")),
        "changed.git was contacted"
    );
}

/// A credential-facing environment: a HOME whose global Git helper answers a
/// fixed credential. `resolve_git_grant_with_environment` then binds
/// `credential.use`, and execution re-resolves the same incarnation.
fn credential_environment(home: &Path) -> Arc<leveler_core::EnvSnapshot> {
    credential_environment_with(home, "leveler-final-secret")
}

fn credential_environment_with(home: &Path, password: &str) -> Arc<leveler_core::EnvSnapshot> {
    use std::os::unix::fs::PermissionsExt;
    let helper = home.join("leveler-helper.sh");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\necho username=leveler-test\necho password={password}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        format!("[credential]\n\thelper = {}\n", helper.display()),
    )
    .unwrap();

    let base = leveler_core::environment();
    let leveler_home = std::env::temp_dir().join(format!("leveler-home-{}", std::process::id()));
    std::fs::create_dir_all(&leveler_home).unwrap();
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = base
        .vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            name != "HOME" && !name.starts_with("GIT_")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    vars.push((
        std::ffi::OsString::from("HOME"),
        home.as_os_str().to_os_string(),
    ));
    vars.push((
        std::ffi::OsString::from("LEVELER_HOME"),
        leveler_home.as_os_str().to_os_string(),
    ));
    Arc::new(leveler_core::EnvSnapshot::new(
        vars,
        base.current_dir().to_path_buf(),
        base.temp_dir().to_path_buf(),
    ))
}

/// P0-2: a detached background task outlives the guard that reaps the frozen
/// Git directory, so the runtime refuses to hand it an approved credential
/// rather than leave the value on disk until the operating system's temp
/// cleanup.
#[tokio::test]
async fn background_git_cannot_use_an_approved_credential() {
    use leveler_core::Capability;
    let served = tempfile::tempdir().unwrap();
    let (base, _log) = serve(served.path().to_path_buf());
    let consumer = fixture(served.path(), &base);
    let home = tempfile::tempdir().unwrap();
    let environment = credential_environment(home.path());
    let grant = leveler_execution::resolve_git_grant_with_environment(
        "git",
        &["fetch".into(), "origin".into()],
        &consumer,
        &environment,
    )
    .await
    .unwrap()
    .expect("an authenticated remote must resolve a grant");
    assert!(
        grant
            .bindings
            .iter()
            .any(|binding| binding.capability == Capability::CredentialUse),
        "the fixture must bind credential.use"
    );

    let workspace = Workspace::new(&consumer).unwrap();
    let root = workspace.root().to_path_buf();
    let policy = ResolvedExecutionPolicy::new(
        WriteScope::WorkspaceWithGit { root },
        NetworkScope::Internet,
        AuthorizationEvidence::ResourceGrant {
            request: grant,
            scope: GrantScope::Once,
        },
    );
    let ctx = ToolContext::with_environment(workspace, PermissionProfile::Assisted, environment)
        .with_resolved_policy(policy);
    let (is_error, content) = run(
        ctx,
        serde_json::json!({"program":"git","args":["fetch","origin"],"background":true}),
    )
    .await
    .expect("the refusal is a tool error, not a dispatch failure");
    assert!(
        is_error,
        "a background credential use must be refused: {content}"
    );
    assert!(
        content.contains("foreground"),
        "the refusal must tell the model how to proceed: {content}"
    );
}

/// The one secret this test plants, in a fixture only. Any occurrence outside
/// the private execution credential file is a leak.
const LEAK_SECRET: &str = "LEVELER_PERMISSION_FINAL_SECRET_7d41c9";

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        out.push(TABLE[(b[0] >> 2) as usize] as char);
        out.push(TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(b[2] & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A dumb-HTTP Git server that requires HTTP Basic and records every request
/// together with the `Authorization` header it saw.
fn serve_auth(root: std::path::PathBuf) -> (String, Arc<Mutex<Vec<(String, Option<String>)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorded = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = root.clone();
            let recorded = recorded.clone();
            std::thread::spawn(move || handle_auth(stream, &root, &recorded));
        }
    });
    (format!("http://{address}"), log)
}

fn handle_auth(
    mut stream: TcpStream,
    root: &Path,
    log: &Arc<Mutex<Vec<(String, Option<String>)>>>,
) {
    let mut buffer = [0u8; 8192];
    let Ok(read) = stream.read(&mut buffer) else {
        return;
    };
    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
    let Some(line) = request.lines().next() else {
        return;
    };
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let authorization = request.lines().find_map(|line| {
        line.split_once(':').and_then(|(name, value)| {
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_string())
        })
    });
    log.lock()
        .unwrap()
        .push((path.clone(), authorization.clone()));
    let expected = format!(
        "Basic {}",
        base64(format!("leveler-test:{LEAK_SECRET}").as_bytes())
    );
    if authorization.as_deref() != Some(&expected) {
        let _ = stream.write_all(
            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"leveler\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    let file = path
        .split('?')
        .next()
        .unwrap_or("/")
        .trim_start_matches('/');
    match std::fs::read(root.join(file)) {
        Ok(body) => {
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            let _ = stream.write_all(&response);
        }
        Err(_) => {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    }
}

/// The full credential workflow through the real tool path: the frozen fetch
/// authenticates with the approved credential, and the secret reaches none of
/// the surfaces the model, the transcript, or the runtime can observe. The
/// private execution directory is reaped when the call returns.
#[tokio::test]
async fn authenticated_fetch_never_exposes_the_secret_to_any_observed_surface() {
    use leveler_core::Capability;
    let served = tempfile::tempdir().unwrap();
    let (base, log) = serve_auth(served.path().to_path_buf());
    let consumer = fixture(served.path(), &base);
    let home = tempfile::tempdir().unwrap();
    let environment = credential_environment_with(home.path(), LEAK_SECRET);
    let grant = leveler_execution::resolve_git_grant_with_environment(
        "git",
        &["fetch".into(), "origin".into()],
        &consumer,
        &environment,
    )
    .await
    .unwrap()
    .expect("an authenticated remote must resolve a grant");
    assert!(
        grant
            .bindings
            .iter()
            .any(|binding| binding.capability == Capability::CredentialUse),
        "the fixture must bind credential.use"
    );
    assert!(
        !serde_json::to_string(&grant).unwrap().contains(LEAK_SECRET),
        "the persisted grant must not carry the secret"
    );

    let workspace = Workspace::new(&consumer).unwrap();
    let root = workspace.root().to_path_buf();
    let policy = ResolvedExecutionPolicy::new(
        WriteScope::WorkspaceWithGit { root },
        NetworkScope::Internet,
        AuthorizationEvidence::ResourceGrant {
            request: grant,
            scope: GrantScope::Once,
        },
    );
    let ctx = ToolContext::with_environment(workspace, PermissionProfile::Assisted, environment)
        .with_resolved_policy(policy);
    let (is_error, content) = run(
        ctx,
        serde_json::json!({"program":"git","args":["fetch","origin"]}),
    )
    .await
    .expect("the frozen fetch must execute");
    assert!(!is_error, "authenticated fetch failed: {content}");

    // The tool result and its command/argv cannot carry the value.
    assert!(
        !content.contains(LEAK_SECRET),
        "secret leaked to the tool result"
    );
    assert!(
        log.lock().unwrap().iter().any(|(_, auth)| auth.as_deref()
            == Some(&format!(
                "Basic {}",
                base64(format!("leveler-test:{LEAK_SECRET}").as_bytes())
            ))),
        "the server never saw the approved credential"
    );

    // No staged credential or isolation directory survives the call.
    let mut leftovers = Vec::new();
    for entry in std::fs::read_dir(std::env::temp_dir()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("leveler-git-") {
            leftovers.push(entry.path());
        }
    }
    assert!(
        leftovers.is_empty(),
        "the foreground call must reap its private Git directory: {leftovers:?}"
    );
}
