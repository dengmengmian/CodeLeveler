//! Repository-facing REST endpoints for the WebUI panels.
//!
//! All five handlers resolve the session's repository through the runtime
//! snapshot and then work strictly inside it: the file viewer reads one text
//! file (`/file`), the file panel lists repository paths (`/files`), the
//! search panel greps case-insensitively (`/search`), the git panel
//! summarizes `git status` (`/git-status`), and the composer accepts
//! multipart uploads stored under `.leveler/uploads` and announced to the
//! runtime as `AddAttachment` commands (`/attachments`). Listing and search
//! respect `.gitignore` (via the `ignore` crate) and never descend into
//! `.git`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;

use axum::Json;
use axum::extract::multipart::Field;
use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use leveler_client_protocol::{
    ClientCommand, ClientError, CommandEnvelope, CommandId, ProtocolEnvelope, SessionId,
};
use leveler_core::new_uuid_string;

use crate::server::AppState;

/// The viewer never receives more than this many bytes of one file.
const MAX_CONTENT_BYTES: usize = 512 * 1024;
/// Above this size the file is not read whole just to count lines; the
/// reported `total_lines` then covers only the returned head.
const FULL_COUNT_CAP: u64 = 16 * 1024 * 1024;
/// Default and fallback caps for the list/search panels.
const DEFAULT_LIST_LIMIT: usize = 2000;
const DEFAULT_SEARCH_LIMIT: usize = 100;
/// Files larger than this are skipped by content search.
const SEARCH_FILE_SIZE_CAP: u64 = 1024 * 1024;
/// A matched line is reported with at most this many characters.
const MATCH_TEXT_MAX_CHARS: usize = 200;
/// Per-file ceiling for one multipart upload.
const MAX_UPLOAD_BYTES: u64 = 20 * 1024 * 1024;

/// `GET /api/sessions/{id}/file?path=<repo-relative>` — one text file for the
/// viewer, capped at 512 KiB and cut at a line boundary when larger.
pub(crate) async fn read_file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileContentResponse>, EndpointError> {
    let root = repository_root(&state, &id).await?;
    // Reject rooted/absolute inputs outright (cross-platform): on Windows an
    // absolute-looking "/etc/hosts" is not `is_absolute()` but does have a root,
    // and `join` would silently reinterpret it against the current drive.
    if std::path::Path::new(&query.path).has_root() {
        return Err(EndpointError::new(
            StatusCode::FORBIDDEN,
            format!("{} is not repository-relative", query.path),
        ));
    }
    let target = root
        .join(&query.path)
        .canonicalize()
        .map_err(|error| io_error(&error, &format!("cannot resolve {}", query.path)))?;
    if !target.starts_with(&root) {
        return Err(EndpointError::new(
            StatusCode::FORBIDDEN,
            format!("{} escapes the repository", query.path),
        ));
    }
    let metadata = tokio::fs::metadata(&target)
        .await
        .map_err(|error| io_error(&error, &format!("cannot read {}", query.path)))?;
    if metadata.is_dir() {
        return Err(EndpointError::new(
            StatusCode::BAD_REQUEST,
            format!("{} is a directory", query.path),
        ));
    }
    if !metadata.is_file() {
        return Err(EndpointError::new(
            StatusCode::BAD_REQUEST,
            format!("{} is not a regular file", query.path),
        ));
    }
    let read = read_text_file(&target, metadata.len(), &query.path).await?;
    Ok(Json(FileContentResponse {
        path: query.path,
        content: read.content,
        truncated: read.truncated,
        total_lines: read.total_lines,
    }))
}

/// `GET /api/sessions/{id}/files?prefix=<optional>&limit=<optional>` — the
/// sorted repository-relative paths behind the file panel.
pub(crate) async fn list_files(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<FilesQuery>,
) -> Result<Json<FileListResponse>, EndpointError> {
    let root = repository_root(&state, &id).await?;
    let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let prefix = query.prefix;
    let files = tokio::task::spawn_blocking(move || collect_files(&root, prefix.as_deref(), limit))
        .await
        .map_err(|error| {
            EndpointError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("file walk failed: {error}"),
            )
        })?;
    Ok(Json(FileListResponse { files }))
}

/// `GET /api/sessions/{id}/search?q=<needle>&limit=<optional>` —
/// case-insensitive substring matches across the repository.
pub(crate) async fn search_files(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchMatchesResponse>, EndpointError> {
    let root = repository_root(&state, &id).await?;
    let needle = query.q.to_lowercase();
    let limit = query.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    if needle.is_empty() || limit == 0 {
        return Ok(Json(SearchMatchesResponse {
            matches: Vec::new(),
        }));
    }
    let matches = tokio::task::spawn_blocking(move || collect_matches(&root, &needle, limit))
        .await
        .map_err(|error| {
            EndpointError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("search failed: {error}"),
            )
        })?;
    Ok(Json(SearchMatchesResponse { matches }))
}

/// `GET /api/sessions/{id}/git-status` — the branch plus per-file status and
/// numstat for the git panel. Anything but a working git repository yields
/// the empty summary (200), never an error.
pub(crate) async fn git_status(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<GitStatusResponse>, EndpointError> {
    let root = repository_root(&state, &id).await?;
    let empty = || {
        Json(GitStatusResponse {
            branch: None,
            files: Vec::new(),
        })
    };
    let Some(status) = run_git(&root, &["status", "--porcelain=v1", "--branch"]).await else {
        return Ok(empty());
    };
    if !status.status.success() {
        return Ok(empty());
    }
    let (branch, entries) = parse_porcelain(&String::from_utf8_lossy(&status.stdout));
    let numstat = match run_git(&root, &["diff", "--numstat", "HEAD", "--"]).await {
        Some(output) if output.status.success() => {
            parse_numstat(&String::from_utf8_lossy(&output.stdout))
        }
        _ => HashMap::new(),
    };
    let files = entries
        .into_iter()
        .map(|(path, status)| {
            let (added, removed) = numstat.get(&path).copied().unwrap_or((0, 0));
            GitFileStatus {
                path,
                status,
                added,
                removed,
            }
        })
        .collect();
    Ok(Json(GitStatusResponse { branch, files }))
}

/// `POST /api/sessions/{id}/attachments` (multipart, `file` fields) — store
/// each upload under `.leveler/uploads` and deliver one `AddAttachment`
/// command per stored file.
pub(crate) async fn upload_attachments(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<StoredAttachmentsResponse>), EndpointError> {
    let root = repository_root(&state, &id).await?;
    let uploads = root.join(".leveler").join("uploads");
    tokio::fs::create_dir_all(&uploads)
        .await
        .map_err(|error| io_error(&error, "cannot create the uploads directory"))?;

    let mut stored = Vec::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| EndpointError::new(StatusCode::BAD_REQUEST, error.to_string()))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let name = field
            .file_name()
            .and_then(|name| {
                Path::new(name)
                    .file_name()
                    .map(|base| base.to_string_lossy().into_owned())
            })
            .filter(|name| !name.is_empty());
        let Some(name) = name else {
            return Err(EndpointError::new(
                StatusCode::BAD_REQUEST,
                "uploaded file has no usable file name",
            ));
        };
        let path = uploads.join(format!("{}-{name}", &new_uuid_string()[..8]));
        store_field(&mut field, &path, &name).await?;
        let absolute = path.to_string_lossy().into_owned();
        deliver_attachment(&state, SessionId::new(id.clone()), absolute.clone())
            .await
            .map_err(|error| {
                EndpointError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("stored {name} but delivering it to the runtime failed: {error}"),
                )
            })?;
        stored.push(absolute);
    }
    if stored.is_empty() {
        return Err(EndpointError::new(
            StatusCode::BAD_REQUEST,
            "multipart body contained no `file` fields",
        ));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(StoredAttachmentsResponse { stored }),
    ))
}

/// Resolve the session's repository root, canonicalized so later
/// `starts_with` checks compare like with like (and symlink escapes fail).
async fn repository_root(state: &AppState, session_id: &str) -> Result<PathBuf, EndpointError> {
    let snapshot = state.service.snapshot(&SessionId::new(session_id)).await?;
    PathBuf::from(&snapshot.repository)
        .canonicalize()
        .map_err(|error| io_error(&error, "repository unavailable"))
}

/// The walk shared by listing and search: `.gitignore` (and `.ignore`) rules
/// are honored even outside a git worktree, dotfiles stay visible, `.git`
/// itself is never entered.
fn repo_walker(root: &Path) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    builder.build()
}

/// Sorted repository-relative file paths, filtered by `prefix` and capped at
/// `limit` after sorting so truncation is deterministic.
fn collect_files(root: &Path, prefix: Option<&str>, limit: usize) -> Vec<String> {
    let mut files = Vec::new();
    for entry in repo_walker(root).flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        // Repo-relative paths use forward slashes on every platform so the WebUI
        // (and the API contract) never sees Windows backslashes.
        let relative = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if prefix.is_some_and(|prefix| !relative.starts_with(prefix)) {
            continue;
        }
        files.push(relative);
    }
    files.sort();
    files.truncate(limit);
    files
}

/// Case-insensitive substring matches, sorted by path then line number and
/// capped at `limit`. Non-UTF-8 and oversized files are skipped.
fn collect_matches(root: &Path, needle: &str, limit: usize) -> Vec<SearchMatch> {
    let mut matches = Vec::new();
    for entry in repo_walker(root).flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > SEARCH_FILE_SIZE_CAP {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let relative = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        for (index, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(needle) {
                matches.push(SearchMatch {
                    path: relative.clone(),
                    line: index + 1,
                    text: line.chars().take(MATCH_TEXT_MAX_CHARS).collect(),
                });
            }
        }
        if matches.len() >= limit {
            break;
        }
    }
    matches.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));
    matches.truncate(limit);
    matches
}

/// Run one git command in `root`; `None` when git cannot even be spawned.
async fn run_git(root: &Path, args: &[&str]) -> Option<Output> {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()
}

/// Parse `git status --porcelain=v1 --branch` into the branch (when any) and
/// `(path, status)` pairs; rename paths are reduced to their new name.
fn parse_porcelain(text: &str) -> (Option<String>, Vec<(String, &'static str)>) {
    let mut branch = None;
    let mut entries = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            branch = parse_branch_line(rest);
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let (code, path) = line.split_at(3);
        let Some(status) = classify_status(code) else {
            continue;
        };
        let path = if status == "renamed" {
            path.rsplit_once(" -> ").map_or(path, |(_, new)| new)
        } else {
            path
        };
        entries.push((unquote(path), status));
    }
    (branch, entries)
}

/// The branch header without the upstream/`[ahead N]` decoration; `None` for
/// a detached HEAD.
fn parse_branch_line(rest: &str) -> Option<String> {
    if let Some(name) = rest.strip_prefix("No commits yet on ") {
        return Some(name.trim().to_string());
    }
    if rest.starts_with("HEAD") {
        return None;
    }
    rest.split("...")
        .next()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Map the two-column porcelain code onto the status vocabulary the panel
/// renders. Unrecognized codes are dropped.
fn classify_status(code: &str) -> Option<&'static str> {
    let code = code.trim();
    if code == "??" {
        Some("untracked")
    } else if code.contains('R') {
        Some("renamed")
    } else if code.contains('A') {
        Some("added")
    } else if code.contains('D') {
        Some("deleted")
    } else if code.contains(['M', 'T', 'U']) {
        Some("modified")
    } else {
        None
    }
}

/// Strip the C-style quoting git puts around unusual paths. Octal escapes
/// for non-ASCII names are left as-is; the common cases (`\"`, `\\`) decode.
fn unquote(path: &str) -> String {
    let path = path.trim();
    if path.len() >= 2 && path.starts_with('"') && path.ends_with('"') {
        path[1..path.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        path.to_string()
    }
}

/// Parse `git diff --numstat` into `path → (added, removed)`; binary entries
/// (`-` counts) are dropped, which the caller treats as `(0, 0)`.
fn parse_numstat(text: &str) -> HashMap<String, (u64, u64)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let added = parts.next()?.parse().ok()?;
            let removed = parts.next()?.parse().ok()?;
            let path = parts.next()?.to_string();
            Some((path, (added, removed)))
        })
        .collect()
}

/// Stream one multipart field to `path`, enforcing the per-file cap. The
/// partial file is removed on any failure so a rejected upload never
/// lingers in `.leveler/uploads`.
async fn store_field(field: &mut Field<'_>, path: &Path, name: &str) -> Result<(), EndpointError> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|error| io_error(&error, "cannot store the upload"))?;
    let mut written = 0_u64;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                written += chunk.len() as u64;
                if written > MAX_UPLOAD_BYTES {
                    drop(file);
                    let _ = tokio::fs::remove_file(path).await;
                    return Err(EndpointError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "{name} exceeds the {} MiB upload limit",
                            MAX_UPLOAD_BYTES / 1024 / 1024
                        ),
                    ));
                }
                if let Err(error) = file.write_all(&chunk).await {
                    let _ = tokio::fs::remove_file(path).await;
                    return Err(io_error(&error, "cannot write the upload"));
                }
            }
            Ok(None) => {
                if let Err(error) = file.flush().await {
                    let _ = tokio::fs::remove_file(path).await;
                    return Err(io_error(&error, "cannot finish the upload"));
                }
                return Ok(());
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(path).await;
                return Err(EndpointError::new(
                    StatusCode::BAD_REQUEST,
                    error.to_string(),
                ));
            }
        }
    }
}

/// Deliver one `AddAttachment` for a stored upload, mirroring the WS path:
/// the command rides in a `CommandEnvelope` through `deliver_protocol` so
/// idempotency/version handling is identical to browser-issued commands.
async fn deliver_attachment(
    state: &AppState,
    session_id: SessionId,
    path: String,
) -> Result<(), ClientError> {
    let envelope = CommandEnvelope {
        command_id: CommandId::generate(),
        session_id: session_id.clone(),
        expected_version: None,
        issued_at: leveler_core::now().to_rfc3339(),
        command: ClientCommand::AddAttachment { session_id, path },
    };
    state
        .service
        .deliver_protocol(ProtocolEnvelope::wrap(envelope))
        .await
}

/// One file read for the viewer: content (possibly truncated), the
/// truncation flag, and the best available total line count.
struct FileRead {
    content: String,
    truncated: bool,
    total_lines: usize,
}

/// Read `path` for the viewer: whole file up to `FULL_COUNT_CAP` (so the
/// line count is exact even when the content is truncated), otherwise just
/// the head. Content over 512 KiB is cut at a line boundary; anything that
/// does not decode as UTF-8 is rejected as 415.
async fn read_text_file(path: &Path, size: u64, display: &str) -> Result<FileRead, EndpointError> {
    let truncated = size > MAX_CONTENT_BYTES as u64;
    let bytes = if size <= FULL_COUNT_CAP {
        tokio::fs::read(path)
            .await
            .map_err(|error| io_error(&error, display))?
    } else {
        let mut head = Vec::new();
        tokio::fs::File::open(path)
            .await
            .map_err(|error| io_error(&error, display))?
            .take(MAX_CONTENT_BYTES as u64)
            .read_to_end(&mut head)
            .await
            .map_err(|error| io_error(&error, display))?;
        head
    };
    let head_len = if truncated {
        line_boundary(&bytes, MAX_CONTENT_BYTES)
    } else {
        bytes.len()
    };
    let total_lines = count_lines(&bytes);
    let content = decode_utf8(&bytes[..head_len], truncated)?;
    Ok(FileRead {
        content,
        truncated,
        total_lines,
    })
}

/// The longest prefix of `bytes` within `max` bytes that ends right after a
/// newline; `max` itself when the head contains no newline at all.
fn line_boundary(bytes: &[u8], max: usize) -> usize {
    let len = bytes.len().min(max);
    match bytes[..len].iter().rposition(|&byte| byte == b'\n') {
        Some(index) => index + 1,
        None => len,
    }
}

/// Logical line count: trailing newline terminates the last line, an empty
/// file has zero lines.
fn count_lines(bytes: &[u8]) -> usize {
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count();
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// Decode the viewer payload as UTF-8. A truncation cut may split a
/// multi-byte character at the very end; that partial tail is dropped.
/// Anything else invalid means the file is not text: 415.
fn decode_utf8(bytes: &[u8], truncated: bool) -> Result<String, EndpointError> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_owned()),
        Err(error) if truncated && error.error_len().is_none() && error.valid_up_to() > 0 => {
            Ok(String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned())
        }
        Err(_) => Err(EndpointError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "file is not valid UTF-8",
        )),
    }
}

/// Render an io failure as the closest HTTP status.
fn io_error(error: &io::Error, context: &str) -> EndpointError {
    let status = match error.kind() {
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    EndpointError::new(status, format!("{context}: {error}"))
}

/// Query string of [`read_file`].
#[derive(Debug, Deserialize)]
pub(crate) struct FileQuery {
    path: String,
}

/// Query string of [`list_files`].
#[derive(Debug, Deserialize)]
pub(crate) struct FilesQuery {
    prefix: Option<String>,
    limit: Option<usize>,
}

/// Query string of [`search_files`].
#[derive(Debug, Deserialize)]
pub(crate) struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

/// Response of [`read_file`].
#[derive(Debug, Serialize)]
pub(crate) struct FileContentResponse {
    path: String,
    content: String,
    truncated: bool,
    total_lines: usize,
}

/// Response of [`list_files`].
#[derive(Debug, Serialize)]
pub(crate) struct FileListResponse {
    files: Vec<String>,
}

/// Response of [`search_files`].
#[derive(Debug, Serialize)]
pub(crate) struct SearchMatchesResponse {
    matches: Vec<SearchMatch>,
}

/// One content-search hit.
#[derive(Debug, Serialize)]
pub(crate) struct SearchMatch {
    path: String,
    line: usize,
    text: String,
}

/// Response of [`git_status`].
#[derive(Debug, Serialize)]
pub(crate) struct GitStatusResponse {
    branch: Option<String>,
    files: Vec<GitFileStatus>,
}

/// One path in the git-status summary.
#[derive(Debug, Serialize)]
pub(crate) struct GitFileStatus {
    path: String,
    status: &'static str,
    added: u64,
    removed: u64,
}

/// Response of [`upload_attachments`]: the absolute paths that were stored.
#[derive(Debug, Serialize)]
pub(crate) struct StoredAttachmentsResponse {
    stored: Vec<String>,
}

/// A repository-endpoint failure rendered as a status plus the shared
/// `{"error": …}` body.
#[derive(Debug)]
pub(crate) struct EndpointError {
    status: StatusCode,
    message: String,
}

impl EndpointError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl From<ClientError> for EndpointError {
    fn from(error: ClientError) -> Self {
        let status = match &error {
            ClientError::SessionNotFound(_) => StatusCode::NOT_FOUND,
            ClientError::Runtime(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, error.to_string())
    }
}

impl IntoResponse for EndpointError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parses_branch_and_statuses() {
        let (branch, entries) = parse_porcelain(
            "## main...origin/main [ahead 1]\n M src/a.rs\nM  src/b.rs\nA  src/c.rs\nD  src/d.rs\nR  old.rs -> new.rs\n?? fresh.txt\n!! ignored.bin\n",
        );
        assert_eq!(branch.as_deref(), Some("main"));
        assert_eq!(
            entries,
            vec![
                ("src/a.rs".to_string(), "modified"),
                ("src/b.rs".to_string(), "modified"),
                ("src/c.rs".to_string(), "added"),
                ("src/d.rs".to_string(), "deleted"),
                ("new.rs".to_string(), "renamed"),
                ("fresh.txt".to_string(), "untracked"),
            ]
        );
    }

    #[test]
    fn porcelain_handles_detached_and_unborn_heads() {
        let (branch, _) = parse_porcelain("## HEAD (no branch)\n");
        assert_eq!(branch, None);
        let (branch, _) = parse_porcelain("## No commits yet on main\n?? a.rs\n");
        assert_eq!(branch.as_deref(), Some("main"));
    }

    #[test]
    fn numstat_skips_binary_entries() {
        let numstat = parse_numstat("3\t1\tsrc/a.rs\n-\t-\timg.png\n");
        assert_eq!(numstat.get("src/a.rs"), Some(&(3, 1)));
        assert_eq!(numstat.get("img.png"), None);
    }

    #[test]
    fn line_cutting_and_counting_agree() {
        let bytes = b"one\ntwo\nthree\n";
        assert_eq!(line_boundary(bytes, 8), 8); // "one\ntwo\n" ends on a newline
        assert_eq!(line_boundary(bytes, 7), 4); // "one\ntwo" cuts back to "one\n"
        assert_eq!(line_boundary(b"nolinebreak", 100), 11);
        assert_eq!(count_lines(bytes), 3);
        assert_eq!(count_lines(b"one\ntwo"), 2);
        assert_eq!(count_lines(b""), 0);
    }

    #[test]
    fn decode_utf8_trims_only_a_truncation_split() {
        let bytes = "héllo".as_bytes();
        let split = &bytes[..2]; // cuts the two-byte é
        assert_eq!(decode_utf8(split, true).unwrap(), "h");
        assert!(decode_utf8(split, false).is_err());
        assert!(decode_utf8(&[0xff, 0xfe], true).is_err());
    }
}
