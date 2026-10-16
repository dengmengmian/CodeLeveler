//! One publish at a time.
//!
//! Two releases sharing a version, a frozen commit and a dogfood lab would
//! interleave their evidence and could both push. The lock is taken before any
//! state is read and held for the whole transaction. It is not a performance
//! primitive: it is the thing that makes "./dev publish refuses to run twice"
//! true.

use std::path::{Path, PathBuf};

use crate::error::Fail;
use crate::exec::{self, CommandSpec, Host};
use crate::json::Json;
use crate::state;

pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Take the publish lock, or fail with `RELEASE_BUSY`. A lock whose owner
    /// is gone is reclaimed and reported, never silently ignored.
    pub fn acquire(host: &dyn Host, repo: &Path, release_id: &str) -> Result<Lock, Fail> {
        let path = state::lock_path(repo);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                Fail::new(
                    "RELEASE_STATE_IO",
                    format!("could not create {}: {err}", parent.display()),
                )
            })?;
        }
        let payload_for = |reclaimed_from_pid: Option<u32>| {
            Json::obj(vec![
                ("pid", Json::num(std::process::id() as f64)),
                ("release_id", Json::str(release_id.to_string())),
                (
                    "acquired_at",
                    Json::str(crate::clock::iso8601(crate::clock::now())),
                ),
                (
                    "reclaimed_from_pid",
                    reclaimed_from_pid
                        .map(|pid| Json::num(pid as f64))
                        .unwrap_or(Json::Null),
                ),
            ])
            .to_pretty()
        };
        let payload = payload_for(None);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                let _ = file.write_all(payload.as_bytes());
                Ok(Lock { path })
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let owner = std::fs::read_to_string(&path).unwrap_or_default();
                let holder = Json::parse(&owner).ok();
                let pid = holder
                    .as_ref()
                    .and_then(|doc| doc.path_f64("pid"))
                    .map(|value| value as u32);
                let held_by = holder
                    .as_ref()
                    .and_then(|doc| doc.path_str("release_id"))
                    .unwrap_or("unknown")
                    .to_string();
                if let Some(pid) = pid
                    && std::process::id() != pid
                    && process_alive(host, repo, pid)?
                {
                    return Err(Fail::new(
                        "RELEASE_BUSY",
                        format!(
                            "another publish is running: pid {pid}, release {held_by}\n\
                             lock: {}\n\
                             wait for it, or delete that file if you are certain no publish is running",
                            path.display()
                        ),
                    ));
                }
                // The recorded owner is this process, or it is gone: a crashed
                // publish must not block the resume it exists to enable.
                state::write_atomic(&path, &payload_for(pid))?;
                Ok(Lock { path })
            }
            Err(err) => Err(Fail::new(
                "RELEASE_STATE_IO",
                format!("could not take the publish lock {}: {err}", path.display()),
            )),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether the process that took the lock still exists. An unanswerable
/// question is not a licence to proceed: it fails closed.
fn process_alive(host: &dyn Host, repo: &Path, pid: u32) -> Result<bool, Fail> {
    let out = match host.run(&CommandSpec::new(
        "ps",
        ["-p", &pid.to_string(), "-o", "pid="],
        repo,
    )) {
        Ok(out) => out,
        Err(err) => {
            return Err(Fail::new(
                "RELEASE_LOCK_UNVERIFIED",
                format!(
                    "the publish lock exists but its owner (pid {pid}) could not be checked: {}\n\
                     refusing to run two publishes at once; delete {} only if you are certain",
                    err.detail,
                    state::lock_path(repo).display()
                ),
            ));
        }
    };
    Ok(out.status == 0 && !out.stdout.trim().is_empty())
}

/// Confirm the lock file is still the one this process wrote.
pub fn verify_held(repo: &Path) -> Result<(), Fail> {
    let path = state::lock_path(repo);
    let text = std::fs::read_to_string(&path).map_err(|err| {
        Fail::new(
            "RELEASE_LOCK_LOST",
            format!(
                "the publish lock {} disappeared: {err}\n\
                 another process may have reclaimed it; ./dev stopped instead of racing",
                path.display()
            ),
        )
    })?;
    let holder = Json::parse(&text).unwrap_or(Json::Null);
    let pid = holder.path_f64("pid").map(|value| value as u32);
    if pid != Some(std::process::id()) {
        return Err(Fail::new(
            "RELEASE_LOCK_LOST",
            format!(
                "the publish lock is now held by pid {:?}, not this process\n{}",
                pid,
                exec::first_lines(&text, 5)
            ),
        ));
    }
    Ok(())
}
