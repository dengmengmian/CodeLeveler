//! End-to-end tests over a real loopback listener: REST auth, the WS
//! handshake, command delivery acks, event forwarding, and snapshots.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

use leveler_client_protocol::mock::MockRuntimeClient;
use leveler_client_protocol::{
    ClientCommand, ClientError, InteractiveRuntimeClient, NotificationLevel, RuntimeEvent,
    SessionId, UiSessionSnapshot,
};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// A `LocalRuntimeService` facade over the protocol crate's mock, so tests
/// drive session creation, commands, snapshots, and events deterministically.
struct TestService {
    mock: MockRuntimeClient,
}

impl TestService {
    fn new() -> Self {
        Self {
            mock: MockRuntimeClient::new(SessionId::new("s1")),
        }
    }
}

#[async_trait]
impl InteractiveRuntimeClient for TestService {
    async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
        self.mock.send(command).await
    }

    fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.mock.subscribe()
    }

    async fn snapshot(&self, session_id: &SessionId) -> Result<UiSessionSnapshot, ClientError> {
        self.mock.snapshot(session_id).await
    }
}

#[async_trait]
impl LocalRuntimeService for TestService {
    async fn create_session(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<SessionBootstrap, ClientError> {
        Ok(SessionBootstrap {
            session: self.mock.snapshot(&SessionId::new("s1")).await?,
            context_window: 4096,
        })
    }
}

/// A running server on an ephemeral loopback port, stopped on drop.
struct TestServer {
    addr: SocketAddr,
    service: Arc<TestService>,
    shutdown: CancellationToken,
}

impl TestServer {
    async fn start() -> Self {
        let service = Arc::new(TestService::new());
        let server = leveler_web::bind(
            service.clone(),
            "127.0.0.1:0".parse().unwrap(),
            TOKEN.to_string(),
        )
        .await
        .unwrap();
        let addr = server.local_addr();
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        tokio::spawn(async move { server.serve(server_shutdown).await });
        Self {
            addr,
            service,
            shutdown,
        }
    }

    fn http(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

type TestSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Read the next text frame as JSON, failing the test on any hiccup.
async fn next_json(socket: &mut TestSocket) -> serde_json::Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out waiting for a frame")
        .expect("the server closed the connection")
        .expect("frame error");
    match message {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

#[tokio::test]
async fn spa_shell_serves_without_a_token() {
    // The app shell is public (only /api and the WS upgrade are token-gated) so
    // the browser can load the page and then present its token. End-to-end over
    // the real server: when the frontend is built the shell is 200 HTML; a
    // backend-only checkout answers 503 with the build hint. Either is correct
    // routing — what must never happen is a 401 or a 5xx crash.
    let server = TestServer::start().await;
    let response = reqwest::get(server.http("/")).await.unwrap();
    let status = response.status();
    assert!(
        status == reqwest::StatusCode::OK || status == reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "SPA shell must route (200 built / 503 unbuilt), got {status}"
    );
    if status == reqwest::StatusCode::OK {
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response.text().await.unwrap();
        assert!(
            content_type.contains("text/html"),
            "shell should be HTML, got {content_type:?}"
        );
        assert!(
            body.contains("<div id=\"root\""),
            "shell should mount the SPA root"
        );
    }
}

#[tokio::test]
async fn rest_rejects_missing_and_wrong_tokens() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let health = server.http("/api/health");
    assert_eq!(
        client.get(&health).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(format!("{health}?token=wrong"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    // The Authorization header is accepted too.
    assert_eq!(
        client
            .get(&health)
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn health_answers_with_query_token_or_bearer_header() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let health = server.http("/api/health");
    for request in [
        client.get(format!("{health}?token={TOKEN}")),
        client.get(&health).bearer_auth(TOKEN),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body, serde_json::json!({ "ok": true }));
    }
}

#[tokio::test]
async fn create_session_returns_the_bootstrap() {
    let server = TestServer::start().await;
    let response = reqwest::Client::new()
        .post(server.http(&format!("/api/sessions?token={TOKEN}")))
        .json(&serde_json::json!({ "goal": "fix tests", "model": null, "mode": "assisted" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["session"]["id"], "s1");
    assert_eq!(body["context_window"], 4096);
}

#[tokio::test]
async fn snapshot_endpoint_returns_the_session() {
    let server = TestServer::start().await;
    let response = reqwest::Client::new()
        .get(server.http(&format!("/api/sessions/s1/snapshot?token={TOKEN}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["id"], "s1");
    assert_eq!(body["goal"], "interactive session");
}

#[tokio::test]
async fn ws_rejects_a_wrong_token_before_upgrade() {
    let server = TestServer::start().await;
    let result = connect_async(format!("ws://{}/ws?token=wrong", server.addr)).await;
    assert!(result.is_err(), "a wrong token must not be upgraded");
}

#[tokio::test]
async fn ws_pushes_snapshot_then_acks_and_forwards_events() {
    let server = TestServer::start().await;
    let (mut socket, _) =
        connect_async(format!("ws://{}/ws?session=s1&token={TOKEN}", server.addr))
            .await
            .unwrap();

    // Greeting frame: the requested session's snapshot.
    let greeting = next_json(&mut socket).await;
    assert_eq!(greeting["type"], "snapshot");
    assert_eq!(greeting["session"]["id"], "s1");

    // deliver → ack, and the runtime records the command.
    let deliver = serde_json::json!({
        "type": "deliver",
        "command_id": "cmd-1",
        "session_id": "s1",
        "command": { "type": "submit_message", "session_id": "s1", "content": "你好", "attachments": [] },
    });
    socket
        .send(Message::Text(deliver.to_string().into()))
        .await
        .unwrap();
    let ack = next_json(&mut socket).await;
    assert_eq!(
        ack,
        serde_json::json!({ "type": "ack", "command_id": "cmd-1" })
    );
    let commands = server.service.mock.commands();
    assert_eq!(commands.len(), 1);
    assert!(
        matches!(&commands[0], ClientCommand::SubmitMessage { content, .. } if content == "你好")
    );

    // An event emitted by the runtime arrives as an event frame.
    server.service.mock.emit(RuntimeEvent::Notification {
        level: NotificationLevel::Info,
        message: "hi".to_string(),
    });
    let event = next_json(&mut socket).await;
    assert_eq!(
        event,
        serde_json::json!({
            "type": "event",
            "event": { "type": "notification", "level": "info", "message": "hi" },
        })
    );
}

#[tokio::test]
async fn ws_reports_bad_frames_and_stays_connected() {
    let server = TestServer::start().await;
    let (mut socket, _) = connect_async(format!("ws://{}/ws?token={TOKEN}", server.addr))
        .await
        .unwrap();

    socket.send(Message::Text("not json".into())).await.unwrap();
    let error = next_json(&mut socket).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["code"], "invalid_frame");
    assert!(error["command_id"].is_null());

    // The connection survives: a snapshot request is still answered.
    let request = serde_json::json!({ "type": "snapshot", "session_id": "s1" });
    socket
        .send(Message::Text(request.to_string().into()))
        .await
        .unwrap();
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["session"]["id"], "s1");
}

#[tokio::test]
async fn bind_rejects_empty_token_and_non_loopback_addresses() {
    let service = Arc::new(TestService::new());
    let result = leveler_web::bind(
        service.clone(),
        "127.0.0.1:0".parse().unwrap(),
        String::new(),
    )
    .await;
    assert!(matches!(result, Err(leveler_web::WebError::EmptyToken)));
    let result = leveler_web::bind(service, "0.0.0.0:0".parse().unwrap(), TOKEN.to_string()).await;
    assert!(matches!(
        result,
        Err(leveler_web::WebError::NonLoopback(addr)) if !addr.ip().is_loopback()
    ));
}

// ---------------------------------------------------------------------------
// Repository endpoints (file viewer, panels, attachments)
// ---------------------------------------------------------------------------

/// Point the mock session's repository at a real directory on disk.
async fn use_repository(server: &TestServer, repository: &Path) {
    let mut snapshot = server
        .service
        .mock
        .snapshot(&SessionId::new("s1"))
        .await
        .unwrap();
    snapshot.repository = repository.to_string_lossy().into_owned();
    server.service.mock.set_snapshot(snapshot);
}

fn http_get(server: &TestServer, path_and_query: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new().get(server.http(path_and_query))
}

#[tokio::test]
async fn file_endpoint_reads_a_repository_file() {
    let server = TestServer::start().await;
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::write(
        repo.path().join("src/main.rs"),
        "fn main() {}\nsecond line\n",
    )
    .unwrap();
    use_repository(&server, repo.path()).await;

    let response = http_get(
        &server,
        &format!("/api/sessions/s1/file?token={TOKEN}&path=src/main.rs"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["path"], "src/main.rs");
    assert_eq!(body["content"], "fn main() {}\nsecond line\n");
    assert_eq!(body["truncated"], false);
    assert_eq!(body["total_lines"], 2);
}

#[tokio::test]
async fn file_endpoint_rejects_traversal_missing_and_directories() {
    let server = TestServer::start().await;
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    // A sibling of the repository root: one `..` hop escapes the repo.
    let sibling = repo.path().parent().unwrap().join(format!(
        "leveler-web-test-{}-secret.txt",
        std::process::id()
    ));
    std::fs::write(&sibling, "outside").unwrap();
    use_repository(&server, repo.path()).await;

    let escape = format!("../{}", sibling.file_name().unwrap().to_string_lossy());
    for (path, expected) in [
        (escape.as_str(), reqwest::StatusCode::FORBIDDEN),
        ("/etc/hosts", reqwest::StatusCode::FORBIDDEN),
        ("..", reqwest::StatusCode::FORBIDDEN),
        ("nope.rs", reqwest::StatusCode::NOT_FOUND),
        ("src", reqwest::StatusCode::BAD_REQUEST),
    ] {
        let response = http_get(
            &server,
            &format!("/api/sessions/s1/file?token={TOKEN}&path={path}"),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(
            response.status(),
            expected,
            "path {path:?} must map to {expected}"
        );
    }
    let _ = std::fs::remove_file(&sibling);
}

#[tokio::test]
async fn file_endpoint_rejects_non_utf8_content() {
    let server = TestServer::start().await;
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
    use_repository(&server, repo.path()).await;

    let response = http_get(
        &server,
        &format!("/api/sessions/s1/file?token={TOKEN}&path=blob.bin"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
}

#[tokio::test]
async fn file_endpoint_truncates_large_files_at_a_line_boundary() {
    let server = TestServer::start().await;
    let repo = tempfile::tempdir().unwrap();
    let mut big = String::new();
    for index in 0..60_000 {
        big.push_str(&format!("line {index:06}\n"));
    }
    std::fs::write(repo.path().join("big.txt"), &big).unwrap();
    use_repository(&server, repo.path()).await;

    let response = http_get(
        &server,
        &format!("/api/sessions/s1/file?token={TOKEN}&path=big.txt"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["truncated"], true);
    assert_eq!(body["total_lines"], 60_000);
    let content = body["content"].as_str().unwrap();
    assert!(content.len() <= 512 * 1024, "content exceeds the cap");
    assert!(content.starts_with("line 000000\n"));
    assert!(
        content.ends_with('\n'),
        "truncation cuts at a line boundary"
    );
}

#[tokio::test]
async fn files_endpoint_lists_paths_respecting_gitignore() {
    let server = TestServer::start().await;
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::create_dir_all(repo.path().join("target/debug")).unwrap();
    std::fs::create_dir_all(repo.path().join(".git")).unwrap();
    std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(repo.path().join("README.md"), "# readme\n").unwrap();
    std::fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
    std::fs::write(repo.path().join("target/debug/app.o"), [0_u8; 4]).unwrap();
    std::fs::write(repo.path().join(".git/config"), "[core]\n").unwrap();
    use_repository(&server, repo.path()).await;

    let response = http_get(&server, &format!("/api/sessions/s1/files?token={TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    let files: Vec<&str> = body["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file.as_str().unwrap())
        .collect();
    assert_eq!(files, [".gitignore", "README.md", "src/main.rs"]);

    // Prefix narrows the listing.
    let response = http_get(
        &server,
        &format!("/api/sessions/s1/files?token={TOKEN}&prefix=src"),
    )
    .send()
    .await
    .unwrap();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["files"], serde_json::json!(["src/main.rs"]));
}

#[tokio::test]
async fn search_endpoint_matches_case_insensitively() {
    let server = TestServer::start().await;
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("src")).unwrap();
    std::fs::create_dir_all(repo.path().join("target")).unwrap();
    std::fs::write(repo.path().join("src/a.rs"), "Hello World\nsecond line\n").unwrap();
    std::fs::write(repo.path().join("src/b.rs"), "hello again\n").unwrap();
    std::fs::write(
        repo.path().join("target/ignored.rs"),
        "hello in build output\n",
    )
    .unwrap();
    std::fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
    use_repository(&server, repo.path()).await;

    let response = http_get(
        &server,
        &format!("/api/sessions/s1/search?token={TOKEN}&q=HELLO"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    let matches = body["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 2, "gitignored build output must not match");
    assert_eq!(matches[0]["path"], "src/a.rs");
    assert_eq!(matches[0]["line"], 1);
    assert_eq!(matches[0]["text"], "Hello World");
    assert_eq!(matches[1]["path"], "src/b.rs");
    assert_eq!(matches[1]["line"], 1);

    // No hits → an empty array, not an error.
    let response = http_get(
        &server,
        &format!("/api/sessions/s1/search?token={TOKEN}&q=zzz"),
    )
    .send()
    .await
    .unwrap();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["matches"], serde_json::json!([]));
}

#[tokio::test]
async fn git_status_is_empty_outside_a_git_repo() {
    let server = TestServer::start().await;
    let repo = tempfile::tempdir().unwrap();
    use_repository(&server, repo.path()).await;

    let response = http_get(
        &server,
        &format!("/api/sessions/s1/git-status?token={TOKEN}"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body, serde_json::json!({ "branch": null, "files": [] }));
}

/// Run one git command during test setup; false when git is unavailable.
fn git(repo: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn git_status_summarizes_changes_in_a_git_repo() {
    let probe = tempfile::tempdir().unwrap();
    if !git(probe.path(), &["init"]) {
        eprintln!("git is not available; skipping the git-repo case");
        return;
    }
    let server = TestServer::start().await;
    let repo = tempfile::tempdir().unwrap();
    assert!(git(repo.path(), &["init"]));
    std::fs::write(repo.path().join("tracked.txt"), "one\n").unwrap();
    assert!(git(repo.path(), &["add", "tracked.txt"]));
    assert!(git(
        repo.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "init",
        ]
    ));
    std::fs::write(repo.path().join("tracked.txt"), "one\ntwo\n").unwrap();
    std::fs::write(repo.path().join("fresh.rs"), "fn fresh() {}\n").unwrap();
    use_repository(&server, repo.path()).await;

    let response = http_get(
        &server,
        &format!("/api/sessions/s1/git-status?token={TOKEN}"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(
        body["branch"]
            .as_str()
            .is_some_and(|branch| !branch.is_empty()),
        "a git repo reports its branch: {body}"
    );
    let files = body["files"].as_array().unwrap();
    let tracked = files
        .iter()
        .find(|file| file["path"] == "tracked.txt")
        .expect("tracked.txt shows up");
    assert_eq!(tracked["status"], "modified");
    assert_eq!(tracked["added"], 1);
    assert_eq!(tracked["removed"], 0);
    let fresh = files
        .iter()
        .find(|file| file["path"] == "fresh.rs")
        .expect("fresh.rs shows up");
    assert_eq!(fresh["status"], "untracked");
    assert_eq!(fresh["added"], 0);
    assert_eq!(fresh["removed"], 0);
}

// ---------------------------------------------------------------------------
// Filesystem browser (the "open project" modal)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fs_list_lists_subdirectories_and_marks_repos() {
    let server = TestServer::start().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("alpha")).unwrap();
    std::fs::create_dir_all(root.path().join("beta")).unwrap();
    std::fs::create_dir_all(root.path().join("repo/.git")).unwrap();
    std::fs::write(root.path().join("notme.txt"), "x").unwrap();
    let canonical = root.path().canonicalize().unwrap();

    let response = http_get(
        &server,
        &format!(
            "/api/fs/list?token={TOKEN}&path={}",
            canonical.to_string_lossy()
        ),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["path"], canonical.to_string_lossy().as_ref());
    assert_eq!(
        body["parent"],
        canonical.parent().unwrap().to_string_lossy().as_ref()
    );
    let entries = body["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    // Directories only, sorted; the plain file is excluded.
    assert_eq!(names, ["alpha", "beta", "repo"]);
    let repo = entries.iter().find(|e| e["name"] == "repo").unwrap();
    assert_eq!(repo["is_repo"], true);
    let alpha = entries.iter().find(|e| e["name"] == "alpha").unwrap();
    assert_eq!(alpha["is_repo"], false);
    assert_eq!(
        repo["path"],
        canonical.join("repo").to_string_lossy().as_ref()
    );
}

#[tokio::test]
async fn fs_list_rejects_missing_path_and_files() {
    let server = TestServer::start().await;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("a-file.txt");
    std::fs::write(&file, "x").unwrap();
    let missing = root.path().join("does-not-exist");

    for (path, expected) in [
        (file, reqwest::StatusCode::BAD_REQUEST),
        (missing, reqwest::StatusCode::NOT_FOUND),
    ] {
        let response = http_get(
            &server,
            &format!("/api/fs/list?token={TOKEN}&path={}", path.to_string_lossy()),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), expected, "path {path:?}");
    }
}

#[tokio::test]
async fn fs_list_requires_a_token() {
    let server = TestServer::start().await;
    let response = http_get(&server, "/api/fs/list?path=/")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn fs_list_defaults_to_home_when_path_is_absent() {
    let server = TestServer::start().await;
    let response = http_get(&server, &format!("/api/fs/list?token={TOKEN}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(
        body["path"]
            .as_str()
            .is_some_and(|p| std::path::Path::new(p).is_absolute()),
        "defaults to an absolute directory: {body}"
    );
}

#[tokio::test]
async fn attachments_store_uploads_and_deliver_commands() {
    let server = TestServer::start().await;
    let repo = tempfile::tempdir().unwrap();
    use_repository(&server, repo.path()).await;
    let uploads = repo.path().canonicalize().unwrap().join(".leveler/uploads");

    let form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"first contents".to_vec()).file_name("note.txt"),
        )
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"second contents".to_vec()).file_name("../escape.md"),
        );
    let response = reqwest::Client::new()
        .post(server.http(&format!("/api/sessions/s1/attachments?token={TOKEN}")))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let body: serde_json::Value = response.json().await.unwrap();
    let stored: Vec<&str> = body["stored"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    assert_eq!(stored.len(), 2);
    assert!(stored[0].starts_with(uploads.to_string_lossy().as_ref()));
    assert!(stored[0].ends_with("-note.txt"));
    assert_eq!(
        std::fs::read_to_string(stored[0]).unwrap(),
        "first contents"
    );
    assert!(
        stored[1].ends_with("-escape.md"),
        "the file name is reduced to its basename: {}",
        stored[1]
    );
    assert_eq!(
        std::fs::read_to_string(stored[1]).unwrap(),
        "second contents"
    );

    // Each stored file produced one AddAttachment command carrying its path.
    let commands = server.service.mock.commands();
    assert_eq!(commands.len(), 2);
    for (command, path) in commands.iter().zip(stored) {
        assert!(
            matches!(
                command,
                ClientCommand::AddAttachment { path: delivered, session_id }
                    if delivered == path && session_id.as_str() == "s1"
            ),
            "expected AddAttachment for {path}, got {command:?}"
        );
    }
}
