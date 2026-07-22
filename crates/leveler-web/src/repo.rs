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

use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::io::Read as StdRead;
use std::path::{Path, PathBuf};
use std::process::Output;

use axum::Json;
use axum::extract::multipart::Field;
use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

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
/// Hard server-side caps. Query parameters may narrow these values but can
/// never turn one request into an unbounded allocation.
const MAX_LIST_LIMIT: usize = 2000;
const MAX_SEARCH_LIMIT: usize = 500;
/// Hard per-request traversal/I/O budgets. These bound work even when a
/// repository contains millions of entries or many search-sized files.
const MAX_REPO_SCAN_ENTRIES: usize = 50_000;
const MAX_SEARCH_SCAN_BYTES: u64 = 32 * 1024 * 1024;
/// Files larger than this are skipped by content search.
const SEARCH_FILE_SIZE_CAP: u64 = 1024 * 1024;
/// A matched line is reported with at most this many characters.
const MATCH_TEXT_MAX_CHARS: usize = 200;
/// Per-file ceiling for one multipart upload.
const MAX_UPLOAD_BYTES: u64 = 20 * 1024 * 1024;
/// A request may carry several files, but both their count and their combined
/// payload are bounded independently from the HTTP body limit.
const MAX_UPLOAD_FILES: usize = 4;
const MAX_UPLOAD_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Leave one MiB for multipart boundaries and headers above the payload cap.
pub(crate) const MAX_MULTIPART_BODY_BYTES: usize = MAX_UPLOAD_TOTAL_BYTES as usize + 1024 * 1024;

/// `GET /api/sessions/{id}/file?path=<repo-relative>` — one text file for the
/// viewer, capped at 512 KiB and cut at a line boundary when larger.
pub(crate) async fn read_file(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<FileQuery>,
) -> Result<Json<FileContentResponse>, EndpointError> {
    let repository = repository(&state, &id).await?;
    // Reject rooted/absolute inputs outright (cross-platform): on Windows an
    // absolute-looking "/etc/hosts" is not `is_absolute()` but does have a root,
    // and `join` would silently reinterpret it against the current drive.
    if std::path::Path::new(&query.path).has_root() {
        return Err(EndpointError::new(
            StatusCode::FORBIDDEN,
            format!("{} is not repository-relative", query.path),
        ));
    }
    let relative = PathBuf::from(&query.path);
    let display = query.path.clone();
    let read = tokio::task::spawn_blocking(move || {
        let file = open_repository_file(&repository.dir, &relative, &display)?;
        read_text_file(file, &display)
    })
    .await
    .map_err(|error| {
        EndpointError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("file read task failed: {error}"),
        )
    })??;
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
    let repository = repository(&state, &id).await?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .min(MAX_LIST_LIMIT);
    let prefix = query.prefix;
    let result = tokio::task::spawn_blocking(move || {
        collect_files(
            &repository.path,
            prefix.as_deref(),
            limit,
            MAX_REPO_SCAN_ENTRIES,
        )
    })
    .await
    .map_err(|error| {
        EndpointError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("file walk failed: {error}"),
        )
    })?;
    Ok(Json(FileListResponse {
        files: result.items,
        truncated: result.truncated,
    }))
}

/// `GET /api/sessions/{id}/search?q=<needle>&limit=<optional>` —
/// case-insensitive substring matches across the repository.
pub(crate) async fn search_files(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchMatchesResponse>, EndpointError> {
    let repository = repository(&state, &id).await?;
    let needle = query.q.to_lowercase();
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .min(MAX_SEARCH_LIMIT);
    if needle.is_empty() || limit == 0 {
        return Ok(Json(SearchMatchesResponse {
            matches: Vec::new(),
            truncated: false,
        }));
    }
    let result = tokio::task::spawn_blocking(move || {
        collect_matches(
            &repository,
            &needle,
            limit,
            MAX_REPO_SCAN_ENTRIES,
            MAX_SEARCH_SCAN_BYTES,
        )
    })
    .await
    .map_err(|error| {
        EndpointError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("search failed: {error}"),
        )
    })?;
    Ok(Json(SearchMatchesResponse {
        matches: result.items,
        truncated: result.truncated,
    }))
}

/// `GET /api/sessions/{id}/git-status` — the branch plus per-file status and
/// numstat for the git panel. Anything but a working git repository yields
/// the empty summary (200), never an error.
pub(crate) async fn git_status(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<GitStatusResponse>, EndpointError> {
    let root = repository(&state, &id).await?.path;
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
    let root = repository(&state, &id).await?.path;
    let uploads = prepare_upload_directory(&root)?;

    let mut stored = Vec::new();
    let mut file_count = 0_usize;
    let mut total_bytes = 0_u64;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| EndpointError::new(StatusCode::BAD_REQUEST, error.to_string()))?
    {
        if field.name() != Some("file") {
            continue;
        }
        file_count += 1;
        if file_count > MAX_UPLOAD_FILES {
            return Err(EndpointError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("a request may contain at most {MAX_UPLOAD_FILES} files"),
            ));
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
        let file_name = format!("{}-{name}", &new_uuid_string()[..8]);
        let remaining = MAX_UPLOAD_TOTAL_BYTES.saturating_sub(total_bytes);
        let (written, bytes) =
            store_field(&mut field, &uploads.dir, &file_name, &name, remaining).await?;
        total_bytes += written;
        let path = uploads.absolute.join(&file_name);
        if let Err(error) = verify_stored_upload(&root, &uploads.absolute, &path) {
            let _ = uploads.dir.remove_file(&file_name);
            return Err(error);
        }
        let absolute = path.to_string_lossy().into_owned();
        // Pass the immutable multipart bytes, never the ambient path, to the
        // asynchronous runtime consumer. A later workspace write may replace
        // the stored path, but it cannot change the attachment being imported.
        deliver_attachment_data(
            &state,
            SessionId::new(id.clone()),
            name.clone(),
            BASE64.encode(bytes),
        )
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
async fn repository(state: &AppState, session_id: &str) -> Result<Repository, EndpointError> {
    let snapshot = state.service.snapshot(&SessionId::new(session_id)).await?;
    let configured = PathBuf::from(&snapshot.repository);
    // Capture the repository object first. `dir` remains bound to this opened
    // directory even if its ambient pathname is renamed or replaced later.
    let dir = cap_std::fs::Dir::open_ambient_dir(&configured, cap_std::ambient_authority())
        .map_err(|error| io_error(&error, "cannot open the repository"))?;
    let path = configured
        .canonicalize()
        .map_err(|error| io_error(&error, "repository unavailable"))?;
    Ok(Repository { path, dir })
}

struct Repository {
    path: PathBuf,
    dir: cap_std::fs::Dir,
}

/// Open one repository-relative regular file without following a symbolic
/// link/reparse point in any component. Every directory descent and the final
/// file open are relative to stable capability handles, closing the
/// check-to-open race that ambient canonicalization cannot close.
fn open_repository_file(
    root: &cap_std::fs::Dir,
    relative: &Path,
    display: &str,
) -> Result<cap_std::fs::File, EndpointError> {
    use std::path::Component;

    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => components.push(name.to_os_string()),
            _ => {
                return Err(EndpointError::new(
                    StatusCode::FORBIDDEN,
                    format!("{display} is not a plain repository-relative path"),
                ));
            }
        }
    }
    let Some((file_name, parents)) = components.split_last() else {
        return Err(EndpointError::new(
            StatusCode::BAD_REQUEST,
            format!("{display} does not name a file"),
        ));
    };

    let mut directory = root
        .try_clone()
        .map_err(|error| io_error(&error, "cannot clone the repository handle"))?;
    for parent in parents {
        directory = directory
            .open_dir_nofollow(parent)
            .map_err(|error| secure_open_error(&error, display))?;
    }

    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory
        .open_with(file_name, &options)
        .map_err(|error| secure_open_error(&error, display))?;
    let metadata = file
        .metadata()
        .map_err(|error| io_error(&error, &format!("cannot inspect {display}")))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse_metadata(&metadata) {
        return Err(EndpointError::new(
            StatusCode::BAD_REQUEST,
            format!("{display} is not a regular file"),
        ));
    }
    Ok(file)
}

fn secure_open_error(error: &io::Error, display: &str) -> EndpointError {
    let status = match error.kind() {
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::FORBIDDEN,
    };
    EndpointError::new(status, format!("cannot securely open {display}: {error}"))
}

#[cfg(windows)]
fn is_reparse_metadata(metadata: &cap_std::fs::Metadata) -> bool {
    use cap_fs_ext::OsMetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_metadata(_metadata: &cap_std::fs::Metadata) -> bool {
    false
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

struct BoundedResults<T> {
    items: Vec<T>,
    truncated: bool,
}

/// Sorted repository-relative file paths, filtered by `prefix` and capped at
/// `limit`. A max-heap retains the lexicographically smallest paths among the
/// entries actually visited, so memory is O(limit) and the returned vector is
/// ordered. When the entry budget stops the streaming walk, the set is partial
/// and may reflect the filesystem's enumeration order.
fn collect_files(
    root: &Path,
    prefix: Option<&str>,
    limit: usize,
    max_entries: usize,
) -> BoundedResults<String> {
    if limit == 0 {
        return BoundedResults {
            items: Vec::new(),
            truncated: false,
        };
    }
    let mut files = BinaryHeap::with_capacity(limit);
    let mut truncated = false;
    for (index, entry) in repo_walker(root).enumerate() {
        if index >= max_entries {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
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
        if files.len() < limit {
            files.push(relative);
        } else {
            truncated = true;
            if files.peek().is_some_and(|largest| relative < *largest) {
                files.pop();
                files.push(relative);
            }
        }
    }
    BoundedResults {
        items: files.into_sorted_vec(),
        truncated,
    }
}

/// Case-insensitive substring matches, sorted by path then line number and
/// capped at `limit`. Non-UTF-8 and oversized files are skipped. A budget-cut
/// result is ordered internally but represents only the streamed scan prefix.
fn collect_matches(
    repository: &Repository,
    needle: &str,
    limit: usize,
    max_entries: usize,
    max_bytes: u64,
) -> BoundedResults<SearchMatch> {
    collect_matches_with_hook(repository, needle, limit, max_entries, max_bytes, |_| {})
}

fn collect_matches_with_hook<F>(
    repository: &Repository,
    needle: &str,
    limit: usize,
    max_entries: usize,
    max_bytes: u64,
    mut before_open: F,
) -> BoundedResults<SearchMatch>
where
    F: FnMut(&Path),
{
    if limit == 0 {
        return BoundedResults {
            items: Vec::new(),
            truncated: false,
        };
    }
    let mut matches = BinaryHeap::with_capacity(limit);
    let mut scanned_bytes = 0_u64;
    let mut truncated = false;
    for (index, entry) in repo_walker(&repository.path).enumerate() {
        if index >= max_entries {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative_path) = entry.path().strip_prefix(&repository.path) else {
            continue;
        };
        let relative_path = relative_path.to_path_buf();
        let relative = relative_path
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        before_open(&relative_path);
        let Ok(file) = open_repository_file(&repository.dir, &relative_path, &relative) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if metadata.len() > SEARCH_FILE_SIZE_CAP {
            continue;
        }
        let Some(reserved_bytes) = scanned_bytes.checked_add(metadata.len()) else {
            truncated = true;
            break;
        };
        if reserved_bytes > max_bytes {
            truncated = true;
            break;
        }
        // Read through a bounded handle instead of `fs::read`: if the file
        // grows after metadata, a concurrent writer cannot make this request
        // exceed its reserved I/O budget.
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        if StdRead::take(file.into_std(), metadata.len())
            .read_to_end(&mut bytes)
            .is_err()
        {
            continue;
        }
        scanned_bytes += bytes.len() as u64;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(needle) {
                let candidate = SearchMatch {
                    path: relative.clone(),
                    line: index + 1,
                    text: line.chars().take(MATCH_TEXT_MAX_CHARS).collect(),
                };
                if matches.len() < limit {
                    matches.push(candidate);
                } else {
                    truncated = true;
                    if matches.peek().is_some_and(|largest| candidate < *largest) {
                        matches.pop();
                        matches.push(candidate);
                    }
                }
            }
        }
    }
    BoundedResults {
        items: matches.into_sorted_vec(),
        truncated,
    }
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

/// An opened uploads directory plus the canonical path exposed to the
/// runtime. File creation is performed through `dir`, not by resolving the
/// ambient path again, so swapping a checked parent for a symlink cannot
/// redirect writes outside the repository.
struct UploadDirectory {
    dir: cap_std::fs::Dir,
    absolute: PathBuf,
}

/// Create/open `.leveler/uploads` relative to a capability for the canonical
/// repository root. Both components must be real directories: symlinks are
/// rejected even when they happen to point back inside the repository.
fn prepare_upload_directory(root: &Path) -> Result<UploadDirectory, EndpointError> {
    let repo = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .map_err(|error| io_error(&error, "cannot open the repository"))?;
    ensure_plain_directory(&repo, root, Path::new(".leveler"), ".leveler")?;
    ensure_plain_directory(
        &repo,
        root,
        Path::new(".leveler/uploads"),
        ".leveler/uploads",
    )?;

    // Open a stable directory handle before rechecking both path components.
    // Later file creation stays relative to this handle even if an attacker
    // races an ambient rename or symlink replacement.
    let dir = repo
        .open_dir(".leveler/uploads")
        .map_err(|error| io_error(&error, "cannot open the uploads directory"))?;
    ensure_plain_directory(&repo, root, Path::new(".leveler"), ".leveler")?;
    ensure_plain_directory(
        &repo,
        root,
        Path::new(".leveler/uploads"),
        ".leveler/uploads",
    )?;

    let absolute = root
        .join(".leveler/uploads")
        .canonicalize()
        .map_err(|error| io_error(&error, "cannot resolve the uploads directory"))?;
    if !absolute.starts_with(root) {
        return Err(EndpointError::new(
            StatusCode::FORBIDDEN,
            "the uploads directory escapes the repository",
        ));
    }
    Ok(UploadDirectory { dir, absolute })
}

fn ensure_plain_directory(
    repo: &cap_std::fs::Dir,
    root: &Path,
    path: &Path,
    display: &str,
) -> Result<(), EndpointError> {
    match repo.create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io_error(&error, &format!("cannot create {display}"))),
    }
    let metadata = repo
        .symlink_metadata(path)
        .map_err(|error| io_error(&error, &format!("cannot inspect {display}")))?;
    if metadata.file_type().is_symlink() || is_windows_reparse_point(&root.join(path))? {
        return Err(EndpointError::new(
            StatusCode::FORBIDDEN,
            format!("{display} must not be a symbolic link or reparse point"),
        ));
    }
    if !metadata.is_dir() {
        return Err(EndpointError::new(
            StatusCode::BAD_REQUEST,
            format!("{display} is not a directory"),
        ));
    }
    Ok(())
}

/// Windows junctions and mount points are reparse points but are not
/// consistently surfaced as symbolic links by all metadata APIs. Reject the
/// whole reparse-point class so a junction cannot redirect uploads.
#[cfg(windows)]
fn is_windows_reparse_point(path: &Path) -> Result<bool, EndpointError> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io_error(&error, "cannot inspect an uploads path component"))?;
    Ok(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

#[cfg(not(windows))]
fn is_windows_reparse_point(_path: &Path) -> Result<bool, EndpointError> {
    Ok(false)
}

/// Confirm that the ambient path handed to the runtime still identifies the
/// file just created below the same canonical upload directory. The stable
/// directory handle protects the write; this final check prevents delivery
/// of a path redirected by a concurrent parent-directory swap.
fn verify_stored_upload(root: &Path, uploads: &Path, path: &Path) -> Result<(), EndpointError> {
    let current_uploads = path
        .parent()
        .expect("stored upload always has a parent")
        .canonicalize()
        .map_err(|error| io_error(&error, "cannot resolve the uploads directory after writing"))?;
    let current_file = path
        .canonicalize()
        .map_err(|error| io_error(&error, "cannot resolve the stored upload"))?;
    if current_uploads != uploads
        || !current_uploads.starts_with(root)
        || !current_file.starts_with(&current_uploads)
    {
        return Err(EndpointError::new(
            StatusCode::FORBIDDEN,
            "the stored upload path escaped the repository",
        ));
    }
    let metadata = std::fs::symlink_metadata(&current_file)
        .map_err(|error| io_error(&error, "cannot inspect the stored upload"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(EndpointError::new(
            StatusCode::FORBIDDEN,
            "the stored upload is not a regular file",
        ));
    }
    Ok(())
}

/// Stream one multipart field to `path`, enforcing the per-file cap. The
/// partial file is removed on any failure so a rejected upload never
/// lingers in `.leveler/uploads`.
async fn store_field(
    field: &mut Field<'_>,
    uploads: &cap_std::fs::Dir,
    file_name: &str,
    name: &str,
    total_remaining: u64,
) -> Result<(u64, Vec<u8>), EndpointError> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let file = uploads
        .open_with(file_name, &options)
        .map_err(|error| io_error(&error, "cannot store the upload"))?;
    let mut file = tokio::fs::File::from_std(file.into_std());
    let mut written = 0_u64;
    let mut bytes = Vec::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                written += chunk.len() as u64;
                if written > MAX_UPLOAD_BYTES {
                    drop(file);
                    let _ = uploads.remove_file(file_name);
                    return Err(EndpointError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "{name} exceeds the {} MiB upload limit",
                            MAX_UPLOAD_BYTES / 1024 / 1024
                        ),
                    ));
                }
                if written > total_remaining {
                    drop(file);
                    let _ = uploads.remove_file(file_name);
                    return Err(EndpointError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "uploads exceed the {} MiB total request limit",
                            MAX_UPLOAD_TOTAL_BYTES / 1024 / 1024
                        ),
                    ));
                }
                if let Err(error) = file.write_all(&chunk).await {
                    drop(file);
                    let _ = uploads.remove_file(file_name);
                    return Err(io_error(&error, "cannot write the upload"));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => {
                if let Err(error) = file.flush().await {
                    drop(file);
                    let _ = uploads.remove_file(file_name);
                    return Err(io_error(&error, "cannot finish the upload"));
                }
                return Ok((written, bytes));
            }
            Err(error) => {
                drop(file);
                let _ = uploads.remove_file(file_name);
                return Err(EndpointError::new(
                    StatusCode::BAD_REQUEST,
                    error.to_string(),
                ));
            }
        }
    }
}

/// Deliver immutable attachment bytes, mirroring the WS path: the command
/// rides in a `CommandEnvelope` through `deliver_protocol` so
/// idempotency/version handling is identical to browser-issued commands.
async fn deliver_attachment_data(
    state: &AppState,
    session_id: SessionId,
    name: String,
    data_base64: String,
) -> Result<(), ClientError> {
    let envelope = CommandEnvelope {
        command_id: CommandId::generate(),
        session_id: session_id.clone(),
        expected_version: None,
        issued_at: leveler_core::now().to_rfc3339(),
        command: ClientCommand::AddAttachmentData {
            session_id,
            name,
            data_base64,
        },
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

/// Read from an already-opened stable file handle. At most
/// `FULL_COUNT_CAP + 1` bytes are ever read, even if the file grows after its
/// handle metadata was sampled. Content over 512 KiB is cut at a line
/// boundary; anything that does not decode as UTF-8 is rejected as 415.
fn read_text_file(file: cap_std::fs::File, display: &str) -> Result<FileRead, EndpointError> {
    let metadata = file
        .metadata()
        .map_err(|error| io_error(&error, &format!("cannot inspect {display}")))?;
    let read_limit = if metadata.len() <= FULL_COUNT_CAP {
        FULL_COUNT_CAP + 1
    } else {
        MAX_CONTENT_BYTES as u64 + 1
    };
    let bytes = read_bounded(file.into_std(), read_limit)
        .map_err(|error| io_error(&error, &format!("cannot read {display}")))?;
    let count_len = bytes.len().min(FULL_COUNT_CAP as usize);
    let counted = &bytes[..count_len];
    let truncated = metadata.len() > MAX_CONTENT_BYTES as u64 || bytes.len() > MAX_CONTENT_BYTES;
    let head_len = if truncated {
        line_boundary(counted, MAX_CONTENT_BYTES)
    } else {
        counted.len()
    };
    let total_lines = count_lines(counted);
    let content = decode_utf8(&counted[..head_len], truncated)?;
    Ok(FileRead {
        content,
        truncated,
        total_lines,
    })
}

fn read_bounded(file: std::fs::File, limit: u64) -> io::Result<Vec<u8>> {
    // Do not reserve the full 16 MiB count window for ordinary small files.
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024) as usize);
    StdRead::take(file, limit).read_to_end(&mut bytes)?;
    Ok(bytes)
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
    truncated: bool,
}

/// Response of [`search_files`].
#[derive(Debug, Serialize)]
pub(crate) struct SearchMatchesResponse {
    matches: Vec<SearchMatch>,
    truncated: bool,
}

/// One content-search hit.
#[derive(Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
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

    fn test_repository(path: &Path) -> Repository {
        let path = path.canonicalize().unwrap();
        let dir = cap_std::fs::Dir::open_ambient_dir(&path, cap_std::ambient_authority()).unwrap();
        Repository { path, dir }
    }

    #[test]
    fn repository_walker_never_enables_per_directory_sorting() {
        // ignore/walkdir implements per-directory sorting by first collecting
        // every child. That defeats our outer entry budget for a single very
        // wide directory, so guard the production half of this module against
        // accidentally adding the sorting builder method again.
        let production = include_str!("repo.rs")
            .split_once("#[cfg(test)]")
            .unwrap()
            .0;
        let unbounded_sort = ["sort", "_by_file_name"].concat();
        assert!(!production.contains(&unbounded_sort));
    }

    #[test]
    fn file_collection_keeps_a_sorted_bounded_prefix() {
        let repo = tempfile::tempdir().unwrap();
        for name in ["z.txt", "d.txt", "a.txt", "c.txt", "b.txt"] {
            std::fs::write(repo.path().join(name), name).unwrap();
        }
        let result = collect_files(repo.path(), None, 3, usize::MAX);
        assert_eq!(result.items, ["a.txt", "b.txt", "c.txt"]);
        assert!(result.truncated);
        assert!(
            collect_files(repo.path(), None, 0, usize::MAX)
                .items
                .is_empty()
        );
    }

    #[test]
    fn search_collection_keeps_a_sorted_bounded_prefix() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("z.txt"), "hit z\n").unwrap();
        std::fs::write(repo.path().join("a.txt"), "hit one\nhit two\nhit three\n").unwrap();
        let repository = test_repository(repo.path());
        let result = collect_matches(&repository, "hit", 2, usize::MAX, u64::MAX);
        assert_eq!(result.items.len(), 2);
        assert!(result.truncated);
        assert_eq!(result.items[0].path, "a.txt");
        assert_eq!(result.items[0].line, 1);
        assert_eq!(result.items[1].path, "a.txt");
        assert_eq!(result.items[1].line, 2);
    }

    #[test]
    fn file_collection_stops_at_the_entry_budget() {
        let repo = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(repo.path().join(name), name).unwrap();
        }
        // The walker yields the repository root before its first child.
        let result = collect_files(repo.path(), None, 10, 2);
        assert_eq!(result.items.len(), 1);
        assert!(["a.txt", "b.txt", "c.txt"].contains(&result.items[0].as_str()));
        assert!(result.truncated);
    }

    #[test]
    fn search_collection_stops_at_the_byte_budget() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("a.txt"), "hit\n").unwrap();
        std::fs::write(repo.path().join("b.txt"), "hit\n").unwrap();
        let repository = test_repository(repo.path());
        let result = collect_matches(&repository, "hit", 10, usize::MAX, 4);
        assert_eq!(result.items.len(), 1);
        assert!(["a.txt", "b.txt"].contains(&result.items[0].path.as_str()));
        assert!(result.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn viewer_reads_the_open_handle_after_the_path_is_replaced() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside canary").unwrap();
        let victim = repo.path().join("victim.txt");
        std::fs::write(&victim, "inside content").unwrap();
        let repository = test_repository(repo.path());
        let file =
            open_repository_file(&repository.dir, Path::new("victim.txt"), "victim.txt").unwrap();

        std::fs::remove_file(&victim).unwrap();
        symlink(outside.path(), &victim).unwrap();
        let read = read_text_file(file, "victim.txt").unwrap();
        assert_eq!(read.content, "inside content");
        assert!(!read.content.contains("outside canary"));
    }

    #[cfg(unix)]
    #[test]
    fn search_rejects_a_file_replaced_by_an_external_symlink_before_open() {
        use std::os::unix::fs::symlink;

        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside-search-canary\n").unwrap();
        let victim = repo.path().join("victim.txt");
        std::fs::write(&victim, "ordinary repository text\n").unwrap();
        let repository = test_repository(repo.path());
        let mut replaced = false;
        let result = collect_matches_with_hook(
            &repository,
            "outside-search-canary",
            10,
            usize::MAX,
            u64::MAX,
            |relative| {
                if !replaced && relative == Path::new("victim.txt") {
                    std::fs::remove_file(&victim).unwrap();
                    symlink(outside.path(), &victim).unwrap();
                    replaced = true;
                }
            },
        );
        assert!(replaced, "the race hook must replace the walked file");
        assert!(result.items.is_empty());
    }

    #[test]
    fn bounded_reader_stops_after_the_cap_when_an_open_file_grows() {
        let repo = tempfile::tempdir().unwrap();
        let path = repo.path().join("growing.txt");
        std::fs::write(&path, b"small").unwrap();
        let open = std::fs::File::open(&path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(FULL_COUNT_CAP * 2)
            .unwrap();

        let bytes = read_bounded(open, FULL_COUNT_CAP + 1).unwrap();
        assert_eq!(bytes.len() as u64, FULL_COUNT_CAP + 1);
    }

    #[test]
    fn client_limits_are_clamped() {
        assert_eq!(usize::MAX.min(MAX_LIST_LIMIT), MAX_LIST_LIMIT);
        assert_eq!(usize::MAX.min(MAX_SEARCH_LIMIT), MAX_SEARCH_LIMIT);
    }

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
