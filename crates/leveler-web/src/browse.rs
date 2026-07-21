//! Filesystem directory browser for the "open project" modal.
//!
//! Browsers cannot hand JavaScript a real absolute path (the native folder
//! picker only yields an opaque handle), yet registering a project needs one.
//! This token-gated endpoint lets the WebUI walk the *server's* filesystem —
//! which is the user's own machine — one directory at a time, so the modal can
//! offer a click-through picker instead of a raw path field.

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    /// Absolute directory to list. Absent or empty starts at the home dir.
    path: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DirEntry {
    name: String,
    path: String,
    is_repo: bool,
    hidden: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListResponse {
    /// The canonical directory that was listed.
    path: String,
    /// Its parent, or null at the filesystem root.
    parent: Option<String>,
    entries: Vec<DirEntry>,
}

/// `GET /api/fs/list?path=<absolute>` — the immediate sub-directories of one
/// directory (files are omitted), each flagged when it is a git repository.
pub(crate) async fn list_dir(
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse>, Response> {
    let start = match query
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(path) => PathBuf::from(path),
        None => home_dir(),
    };

    let canonical = start.canonicalize().map_err(|error| {
        let status = match error.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::BAD_REQUEST,
        };
        error_response(status, format!("cannot open {}: {error}", start.display()))
    })?;
    if !canonical.is_dir() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("{} is not a directory", canonical.display()),
        ));
    }

    let mut entries = read_subdirectories(&canonical).map_err(|error| {
        error_response(
            StatusCode::FORBIDDEN,
            format!("cannot list {}: {error}", canonical.display()),
        )
    })?;
    // Non-hidden first, then case-insensitive by name — the order the picker shows.
    entries.sort_by(|a, b| {
        a.hidden
            .cmp(&b.hidden)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(Json(ListResponse {
        path: canonical.to_string_lossy().into_owned(),
        parent: canonical
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned()),
        entries,
    }))
}

/// Read one directory's immediate sub-directories. Entries that cannot be
/// stat'd (a race, a broken symlink, a permission hole) are skipped rather
/// than failing the whole listing.
fn read_subdirectories(dir: &Path) -> std::io::Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        // `file_type()` follows no symlink; resolve so a symlinked directory
        // still counts as one and its `.git` probe below is meaningful.
        let is_dir = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        out.push(DirEntry {
            is_repo: path.join(".git").exists(),
            hidden: name.starts_with('.'),
            path: path.to_string_lossy().into_owned(),
            name,
        });
    }
    Ok(out)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}
