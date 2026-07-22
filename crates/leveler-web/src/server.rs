//! HTTP layer: the router, REST endpoints, SPA static assets, and serving.
//!
//! Token-authenticated REST endpoints mirror the daemon's local transport
//! (`health`, `create_session`, `snapshot`) and expose the session
//! repository to the WebUI panels (file viewer, file list, content search,
//! git status, attachment upload — see [`crate::repo`]); everything else is
//! the single-page app: a real asset when the path names one, `index.html`
//! otherwise, so client-side routing works. Assets come from the embedded
//! production build, or from the directory named by `LEVELER_WEB_DIST` when
//! developing the frontend against a live server.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use leveler_client_protocol::{ClientError, SessionId, UiSessionSnapshot};
use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};

use crate::auth;
use crate::projects::ProjectManager;
use crate::router::RouterService;

use super::WebError;

/// Environment variable pointing at an on-disk frontend build (`vite build`
/// output). When set, it takes precedence over the embedded assets so a
/// frontend developer can iterate without recompiling the server.
const DIST_DIR_ENV: &str = "LEVELER_WEB_DIST";

/// The production WebUI build, baked in at compile time. The folder is
/// tolerated as missing (the frontend builds it), so a backend-only checkout
/// still compiles — the SPA then answers with a build hint at runtime.
#[derive(RustEmbed)]
#[folder = "web/dist"]
#[allow_missing = true]
struct Assets;

/// The multi-project aggregation pieces, present when the server was started
/// as an aggregator (`leveler web` over an in-process primary).
#[derive(Clone)]
pub(crate) struct MultiProject {
    pub(crate) router: Arc<RouterService>,
    pub(crate) manager: Arc<ProjectManager>,
}

/// Shared state handed to every handler.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) service: Arc<dyn LocalRuntimeService>,
    pub(crate) token: Arc<str>,
    /// On-disk asset directory override (`LEVELER_WEB_DIST`), if set.
    dist_dir: Option<Arc<Path>>,
    /// `Some` in aggregation mode; `None` for a single-project server
    /// (`--connect` bridge, embedding, tests) — project routes answer 404.
    pub(crate) multi: Option<MultiProject>,
}

impl AppState {
    fn new(
        service: Arc<dyn LocalRuntimeService>,
        token: String,
        multi: Option<MultiProject>,
    ) -> Self {
        let dist_dir = std::env::var_os(DIST_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(PathBuf::into_boxed_path)
            .map(Arc::from);
        Self {
            service,
            token: Arc::from(token),
            dist_dir,
            multi,
        }
    }
}

/// Build the WebUI router over `service`, guarded by `token`. Exposed so
/// embedding binaries (and tests) can mount the routes without `bind`.
pub fn build_router(service: Arc<dyn LocalRuntimeService>, token: String) -> Router {
    build_router_with(AppState::new(service, token, None))
}

/// [`build_router`] in aggregation mode: the [`RouterService`] is the service,
/// and the project routes are live.
pub fn build_router_multi(
    router: Arc<RouterService>,
    manager: Arc<ProjectManager>,
    token: String,
) -> Router {
    let multi = MultiProject {
        router: router.clone(),
        manager,
    };
    build_router_with(AppState::new(router, token, Some(multi)))
}

fn build_router_with(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/sessions", post(create_session))
        .route("/api/sessions/{id}/snapshot", get(session_snapshot))
        .route("/api/sessions/{id}/file", get(crate::repo::read_file))
        .route("/api/sessions/{id}/files", get(crate::repo::list_files))
        .route("/api/sessions/{id}/search", get(crate::repo::search_files))
        .route(
            "/api/sessions/{id}/git-status",
            get(crate::repo::git_status),
        )
        .route(
            "/api/sessions/{id}/attachments",
            post(crate::repo::upload_attachments)
                .layer(DefaultBodyLimit::max(crate::repo::MAX_MULTIPART_BODY_BYTES)),
        )
        .route(
            "/api/projects",
            get(list_projects).post(add_project).delete(remove_project),
        )
        .route("/api/projects/restart", post(restart_project))
        .route("/api/projects/rename", post(rename_project))
        .route("/api/fs/list", get(crate::browse::list_dir))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .merge(api)
        .route("/ws", get(crate::ws::ws_handler))
        .fallback(spa_handler)
        .with_state(state)
}

/// A bound WebUI server: the loopback listener plus the router, ready to
/// serve. Splitting bind from serve lets the caller print the actually-bound
/// address (an ephemeral `:0` port) before blocking.
pub struct WebServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    app: Router,
}

impl WebServer {
    /// The actually-bound address (resolves an ephemeral `:0` port).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve until `shutdown` is cancelled (graceful: in-flight requests and
    /// WebSocket connections are drained by the caller's cancellation).
    pub async fn serve(self, shutdown: CancellationToken) -> Result<(), WebError> {
        axum::serve(self.listener, self.app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await?;
        Ok(())
    }
}

/// Bind the WebUI server on `addr` (must be loopback) with a non-empty bearer
/// `token`.
pub async fn bind(
    service: Arc<dyn LocalRuntimeService>,
    addr: SocketAddr,
    token: String,
) -> Result<WebServer, WebError> {
    if token.is_empty() {
        return Err(WebError::EmptyToken);
    }
    // Loopback only: the token gates every endpoint, but the listener must
    // never be reachable off this machine in the first place.
    if !addr.ip().is_loopback() {
        return Err(WebError::NonLoopback(addr));
    }
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    Ok(WebServer {
        listener,
        local_addr,
        app: build_router(service, token),
    })
}

/// [`bind`] in aggregation mode (multi-project routes + WS status frames).
pub async fn bind_multi(
    router: Arc<RouterService>,
    manager: Arc<ProjectManager>,
    addr: SocketAddr,
    token: String,
) -> Result<WebServer, WebError> {
    if token.is_empty() {
        return Err(WebError::EmptyToken);
    }
    if !addr.ip().is_loopback() {
        return Err(WebError::NonLoopback(addr));
    }
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    Ok(WebServer {
        listener,
        local_addr,
        app: build_router_multi(router, manager, token),
    })
}

/// One-call convenience: [`bind`] and serve until `shutdown` is cancelled.
pub async fn serve(
    service: Arc<dyn LocalRuntimeService>,
    addr: SocketAddr,
    token: String,
    shutdown: CancellationToken,
) -> Result<(), WebError> {
    bind(service, addr, token).await?.serve(shutdown).await
}

/// Token gate for the REST routes. The WS endpoint authenticates itself
/// (query token or Authorization header, checked before the upgrade).
async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if !auth::is_authorized(request.uri().query(), request.headers(), &state.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

/// `POST /api/sessions` body: the transport DTO plus the aggregation layer's
/// optional `project` (canonical repository path). Absent → the primary.
#[derive(Deserialize)]
struct WebCreateSessionRequest {
    #[serde(flatten)]
    inner: CreateSessionRequest,
    #[serde(default)]
    project: Option<String>,
}

async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<WebCreateSessionRequest>,
) -> Result<Json<SessionBootstrap>, ApiError> {
    let bootstrap = match (&state.multi, &request.project) {
        (Some(multi), Some(project)) => {
            multi
                .router
                .create_session_for(Path::new(project), request.inner)
                .await?
        }
        _ => state.service.create_session(request.inner).await?,
    };
    Ok(Json(bootstrap))
}

// ── 多项目管理（聚合层）─────────────────────────────────────────────

#[derive(Deserialize)]
struct ProjectPathRequest {
    path: String,
}

/// `GET /api/projects` — primary first, then registered projects. 404 outside
/// aggregation mode, which the frontend treats as "single-project server".
async fn list_projects(State(state): State<AppState>) -> Response {
    match &state.multi {
        Some(multi) => {
            Json(serde_json::json!({ "projects": multi.manager.list() })).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn add_project(
    State(state): State<AppState>,
    Json(request): Json<ProjectPathRequest>,
) -> Response {
    let Some(multi) = &state.multi else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match multi.manager.add(&request.path).await {
        Ok(project) => Json(serde_json::json!({ "project": project })).into_response(),
        Err(error) => project_error(error),
    }
}

async fn remove_project(
    State(state): State<AppState>,
    Query(request): Query<ProjectPathRequest>,
) -> Response {
    let Some(multi) = &state.multi else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match multi.manager.remove(&request.path) {
        // A JSON body (not 204) — the frontend helper parses every response.
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(error) => project_error(error),
    }
}

async fn restart_project(
    State(state): State<AppState>,
    Json(request): Json<ProjectPathRequest>,
) -> Response {
    let Some(multi) = &state.multi else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match multi.manager.restart(&request.path).await {
        // A JSON body (not 202) — the frontend helper parses every response.
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(error) => project_error(error),
    }
}

#[derive(Deserialize)]
struct RenameProjectRequest {
    path: String,
    /// Display alias; empty restores the path-derived short name.
    name: String,
}

async fn rename_project(
    State(state): State<AppState>,
    Json(request): Json<RenameProjectRequest>,
) -> Response {
    let Some(multi) = &state.multi else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match multi.manager.rename(&request.path, &request.name) {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(error) => project_error(error),
    }
}

fn project_error(error: crate::projects::ProjectError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

async fn session_snapshot(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<UiSessionSnapshot>, ApiError> {
    let snapshot = state.service.snapshot(&SessionId::new(id)).await?;
    Ok(Json(snapshot))
}

/// A client error rendered as an HTTP status plus a small JSON body.
struct ApiError(ClientError);

impl From<ClientError> for ApiError {
    fn from(error: ClientError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            ClientError::SessionNotFound(_) => StatusCode::NOT_FOUND,
            ClientError::Runtime(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = serde_json::json!({ "error": self.0.to_string() });
        (status, Json(body)).into_response()
    }
}

/// Serve the SPA: a built asset when the path names one, else `index.html`
/// (client-side routing). `/api` and `/ws` never reach this handler.
async fn spa_handler(State(state): State<AppState>, method: Method, uri: Uri) -> Response {
    if !matches!(method, Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(response) = serve_asset(&state, path) {
        return response;
    }
    // SPA fallback: any other path renders the app shell.
    serve_index(&state)
}

/// The app shell, or a build hint when no frontend build is available.
fn serve_index(state: &AppState) -> Response {
    match serve_asset(state, "index.html") {
        Some(response) => response,
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "WebUI assets are not built. Run the frontend build, or start the server \
                 with {DIST_DIR_ENV}=<path-to-dist>."
            ),
        )
            .into_response(),
    }
}

/// Resolve one asset: the on-disk override directory first (frontend
/// development), then the embedded production build.
fn serve_asset(state: &AppState, path: &str) -> Option<Response> {
    if let Some(dir) = &state.dist_dir {
        return serve_from_disk(dir, path);
    }
    serve_embedded(path)
}

/// Read an asset from a directory on disk. Path traversal is refused by
/// canonicalizing and requiring the result to stay under the directory.
fn serve_from_disk(dir: &Path, path: &str) -> Option<Response> {
    let root = dir.canonicalize().ok()?;
    let file = dir.join(path).canonicalize().ok()?;
    if !file.starts_with(&root) || !file.is_file() {
        return None;
    }
    let data = std::fs::read(&file).ok()?;
    let mime = mime_guess::from_path(&file).first_or_octet_stream();
    Some(asset_response(mime.as_ref(), data))
}

/// Read an asset from the embedded build. The `mime-guess` rust-embed feature
/// stays off; the type is guessed from the path instead.
fn serve_embedded(path: &str) -> Option<Response> {
    let file = Assets::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    Some(asset_response(mime.as_ref(), file.data.into_owned()))
}

fn asset_response(mime: &str, data: Vec<u8>) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(value) = mime.parse() {
        headers.insert(header::CONTENT_TYPE, value);
    }
    (headers, Body::from(data)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_assets_stay_inside_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<html>ok</html>").unwrap();
        assert!(serve_from_disk(dir.path(), "index.html").is_some());
        assert!(serve_from_disk(dir.path(), "missing.js").is_none());
        assert!(serve_from_disk(dir.path(), "../Cargo.toml").is_none());
        assert!(serve_from_disk(dir.path(), "assets/../../Cargo.toml").is_none());
    }

    #[test]
    fn embedded_index_serves_once_the_frontend_is_built() {
        // Self-adapting to the `allow_missing` embed: when the frontend build is
        // present (the integration contract, and what ships in a release binary)
        // the embedded `index.html` must serve as 200 HTML rather than the
        // "assets not built" 503. A backend-only checkout has no assets, so the
        // assertion is skipped rather than failing the build.
        if Assets::get("index.html").is_none() {
            return;
        }
        let response = serve_embedded("index.html").expect("embedded index.html serves");
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.contains("text/html"),
            "index.html should be served as HTML, got {content_type:?}"
        );
    }

    #[test]
    fn api_error_maps_session_not_found_to_404() {
        let response = ApiError(ClientError::SessionNotFound(SessionId::new("s1"))).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = ApiError(ClientError::Runtime("boom".to_string())).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
