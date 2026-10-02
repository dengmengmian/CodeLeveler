//! Phase 4 — authenticated Git uses the approved credential without the value
//! ever leaving host memory.
//!
//! The fixture is an HTTP Git server that *requires* HTTP Basic auth and
//! records every request together with the `Authorization` header it saw. So
//! "the fetch authenticated with the approved credential" and "the credential
//! never appeared in the command, environment, or output" are observed against
//! a real server, not asserted about an internal flag.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use leveler_core::{Capability, EnvSnapshot, ResourceIdentity};
use leveler_execution::{approved_git_target, isolate_git_command};

const SECRET: &str = "LEVELER_SECRET_LEAK_TEST_9f3a1c";
const USER: &str = "leveler-test";

struct AuthHttp {
    base: String,
    log: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

fn serve_auth(root: PathBuf, expected_basic: String) -> AuthHttp {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorded = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = root.clone();
            let recorded = recorded.clone();
            let expected = expected_basic.clone();
            std::thread::spawn(move || handle(stream, &root, &recorded, &expected));
        }
    });
    AuthHttp {
        base: format!("http://{address}"),
        log,
    }
}

fn handle(
    mut stream: std::net::TcpStream,
    root: &Path,
    log: &Arc<Mutex<Vec<(String, Option<String>)>>>,
    expected: &str,
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
    if authorization.as_deref() != Some(&format!("Basic {expected}")) {
        let response = b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"leveler-test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let _ = stream.write_all(&response);
        return;
    }
    let file = path
        .split('?')
        .next()
        .unwrap_or("/")
        .trim_start_matches('/');
    let response = match std::fs::read(root.join(file)) {
        Ok(body) => {
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            response
        }
        Err(_) => {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
    };
    let _ = stream.write_all(&response);
}

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

/// A home whose global credential helper answers with `password`.
fn credential_home(password: &str) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let helper = home.path().join("leveler-helper.sh");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\necho username={USER}\necho password={password}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.path().join(".gitconfig"),
        format!("[credential]\n\thelper = {}\n", helper.display()),
    )
    .unwrap();
    home
}

fn environment(home: &Path) -> EnvSnapshot {
    // Deliberately no PATH: resolution attributes a snapshot whose executable
    // lookup is the process default, exactly like the production snapshot.
    EnvSnapshot::new(
        [(
            std::ffi::OsString::from("HOME"),
            std::ffi::OsString::from(home.as_os_str()),
        )],
        home.to_path_buf(),
        home.to_path_buf(),
    )
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
    git(
        &consumer,
        &["remote", "add", "origin", &format!("{base}/approved.git")],
    );
    (consumer, approved)
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

async fn resolve(consumer: &Path, home: &Path) -> leveler_core::GrantRequest {
    leveler_execution::resolve_git_grant_with_environment(
        "git",
        &["fetch".into(), "origin".into()],
        consumer,
        &environment(home),
    )
    .await
    .unwrap()
    .expect("an authenticated HTTP remote must resolve a grant")
}

fn target(grant: &leveler_core::GrantRequest) -> leveler_execution::ApprovedGitTarget {
    approved_git_target(grant, "git", &["fetch".into(), "origin".into()])
        .unwrap()
        .expect("the grant must produce an approved target")
}

fn host_of(base: &str) -> String {
    let authority = base
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    authority
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(authority)
        .to_string()
}

#[tokio::test]
async fn authenticated_fetch_uses_the_approved_credential_without_leaking_it() {
    let served = tempfile::tempdir().unwrap();
    let server = serve_auth(
        served.path().to_path_buf(),
        base64(format!("{USER}:{SECRET}").as_bytes()),
    );
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    let home = credential_home(SECRET);
    let grant = resolve(&consumer, home.path()).await;

    let binding = grant
        .bindings
        .iter()
        .find(|binding| binding.capability == Capability::CredentialUse)
        .expect("an authenticated remote must bind credential.use");
    let ResourceIdentity::Credential {
        host,
        identity,
        transport,
        ..
    } = &binding.resource
    else {
        panic!("expected a credential resource, got {:?}", binding.resource);
    };
    assert_eq!(transport, "http");
    assert!(!identity.contains(SECRET), "the grant stored the secret");
    assert_eq!(
        identity,
        &leveler_execution::CredentialMaterial::new(USER, SECRET).identity()
    );
    assert_eq!(host, &host_of(&server.base));

    let target = target(&grant);
    assert!(
        target.credential.is_some(),
        "the target lost its credential"
    );
    let isolation = isolate_git_command(
        &target,
        &["fetch".into(), "origin".into()],
        &consumer,
        &environment(home.path()),
    )
    .await
    .expect("the approved credential must be injectable");

    // The value is nowhere except the private 0600 file.
    assert!(
        !isolation.args().iter().any(|arg| arg.contains(SECRET)),
        "the secret reached the command line: {:?}",
        isolation.args()
    );
    assert!(
        !isolation
            .authority_env()
            .iter()
            .any(|(_, value)| value.contains(SECRET)),
        "the secret reached the child environment: {:?}",
        isolation.authority_env()
    );

    let output = run_isolated(&isolation, &consumer);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "authenticated fetch failed: {stderr}\n{stdout}"
    );
    assert!(!stdout.contains(SECRET), "secret leaked to stdout");
    assert!(!stderr.contains(SECRET), "secret leaked to stderr");

    let requests = server.log.lock().unwrap().clone();
    assert!(
        requests.iter().any(|(_, auth)| auth.as_deref()
            == Some(&format!(
                "Basic {}",
                base64(format!("{USER}:{SECRET}").as_bytes())
            ))),
        "the server never saw the approved credential: {requests:?}"
    );
}

/// P0-2: the staged credential and its private directory live exactly as long
/// as the guard that owns them. Dropping it — normal completion, a failed
/// authentication, or cancellation — removes the secret material rather than
/// leaving it for the operating system's temp cleanup.
#[tokio::test]
async fn the_credential_is_reaped_with_its_isolation_directory() {
    let served = tempfile::tempdir().unwrap();
    let server = serve_auth(
        served.path().to_path_buf(),
        base64(format!("{USER}:{SECRET}").as_bytes()),
    );
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    let home = credential_home(SECRET);
    let grant = resolve(&consumer, home.path()).await;
    let target = target(&grant);
    let isolation = isolate_git_command(
        &target,
        &["fetch".into(), "origin".into()],
        &consumer,
        &environment(home.path()),
    )
    .await
    .expect("the approved credential must be injectable");
    assert!(isolation.carries_credential());
    let root = isolation.root().to_path_buf();
    let secret_file = root.join("credential");
    assert!(
        secret_file.is_file(),
        "the approved credential must be staged"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&secret_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the staged credential must be owner-only");
    }

    drop(isolation);
    assert!(
        !secret_file.exists(),
        "the credential file must be reaped with the guard"
    );
    assert!(
        !root.exists(),
        "the isolation directory must be reaped with the guard"
    );
}

#[tokio::test]
async fn rotating_the_credential_invalidates_the_grant() {
    let served = tempfile::tempdir().unwrap();
    let server = serve_auth(
        served.path().to_path_buf(),
        base64(format!("{USER}:{SECRET}").as_bytes()),
    );
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    let home = credential_home(SECRET);
    let grant = resolve(&consumer, home.path()).await;
    let target = target(&grant);

    // The stored secret changes after approval: the old grant must not
    // authenticate as the new one.
    std::fs::write(
        home.path().join("leveler-helper.sh"),
        format!("#!/bin/sh\necho username={USER}\necho password=ROTATED-{SECRET}\n"),
    )
    .unwrap();
    let error = isolate_git_command(
        &target,
        &["fetch".into(), "origin".into()],
        &consumer,
        &environment(home.path()),
    )
    .await
    .err()
    .expect("a rotated credential must fail closed");
    assert!(error.contains("credential identity changed"), "{error}");
}

#[tokio::test]
async fn a_public_remote_gets_no_credential_binding() {
    let served = tempfile::tempdir().unwrap();
    let server = serve_auth(
        served.path().to_path_buf(),
        base64(format!("{USER}:{SECRET}").as_bytes()),
    );
    let (consumer, _approved) = fixtures_with_base(served.path(), &server.base);
    // A home with no stored credential at all.
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".gitconfig"),
        "[credential]\n\thelper = \"\"\n",
    )
    .unwrap();
    let grant = resolve(&consumer, home.path()).await;
    assert!(
        !grant
            .bindings
            .iter()
            .any(|binding| binding.capability == Capability::CredentialUse),
        "a destination with no stored credential must not prompt"
    );
    assert!(target(&grant).credential.is_none());
}
