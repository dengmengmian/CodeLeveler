//! One locked read-edit-write path for the resident global config.
//!
//! Desktop windows, the TUI, the CLI and the MCP editor are separate
//! processes editing the same `config.toml`. An edit is computed from the
//! current document only while an exclusive advisory lock is held, and the
//! text is verified again before the atomic replace; a writer that bypassed
//! the lock makes the edit recompute instead of overwriting its work. The
//! validator sees the candidate text before anything is written, so an
//! invalid candidate never replaces a working configuration.

use fs2::FileExt;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

/// How many read-compute-write rounds one edit may take before a still-changing
/// file is reported as a conflict instead of overwriting someone else's write.
const MAX_ATTEMPTS: usize = 8;

/// A failed edit. `Apply` and `Validate` carry the caller's own error type;
/// the rest describe the file and lock.
#[derive(Debug)]
pub enum EditError<E> {
    Apply(E),
    Validate(E),
    Parse(String),
    Io(String),
    ConcurrentModification,
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    name.push(".lock");
    path.with_file_name(name)
}

fn read_text(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn parse_text(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>().map_err(|error| {
        crate::global_config::safe_toml_parse_error(text, error.message(), error.span())
    })
}

fn write_document(path: &Path, doc: &DocumentMut) -> Result<(), String> {
    crate::global_config::write_global_config_atomic(path, &doc.to_string())
        .map_err(|error| error.to_string())
}

/// A first-run configuration write. It is the same lock and validator as
/// [`edit`], but it refuses to replace a configuration that another process
/// created while the user was answering the setup prompts.
#[derive(Debug, thiserror::Error)]
pub enum CreateNewError {
    #[error("a configuration was created by another process; nothing was overwritten")]
    AlreadyExists,
    #[error("{0}")]
    Invalid(String),
    #[error("io error: {0}")]
    Io(String),
}

/// Write a complete starter document only if the file is absent or empty.
///
/// An empty document holds no configuration, so replacing it cannot lose a
/// user's setting; a non-empty one is another writer's result and is never
/// overwritten. The advisory lock serializes this with every [`edit`] caller.
pub fn create_new(path: &Path, text: &str) -> Result<(), CreateNewError> {
    let starter = text
        .parse::<DocumentMut>()
        .map_err(|error| CreateNewError::Invalid(error.to_string()))?;
    match edit(
        path,
        |doc| {
            if !doc.is_empty() {
                return Err(CreateNewError::AlreadyExists);
            }
            *doc = starter.clone();
            Ok(())
        },
        |candidate| {
            crate::global_config::GlobalConfig::from_toml_str(candidate)
                .map(|_| ())
                .map_err(|error| CreateNewError::Invalid(error.to_string()))
        },
    ) {
        Ok(()) => Ok(()),
        Err(EditError::Apply(error) | EditError::Validate(error)) => Err(error),
        Err(EditError::Parse(message)) => Err(CreateNewError::Invalid(message)),
        Err(EditError::Io(message)) => Err(CreateNewError::Io(message)),
        Err(EditError::ConcurrentModification) => Err(CreateNewError::AlreadyExists),
    }
}

/// Read, edit and validate the document under the process-shared lock.
///
/// `validate` runs against the candidate text with the same lock held; it is
/// the caller's chance to reject a semantically invalid result before the
/// atomic replace.
pub fn edit<T, E>(
    path: &Path,
    mut apply: impl FnMut(&mut DocumentMut) -> Result<T, E>,
    mut validate: impl FnMut(&str) -> Result<(), E>,
) -> Result<T, EditError<E>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| EditError::Io(format!("{}: {error}", parent.display())))?;
        leveler_core::private_fs::restrict_dir(parent)
            .map_err(|error| EditError::Io(error.to_string()))?;
    }
    let lock_path = lock_path(path);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| EditError::Io(format!("{}: {error}", lock_path.display())))?;
    lock.lock_exclusive()
        .map_err(|error| EditError::Io(format!("{}: {error}", lock_path.display())))?;
    let result = (|| {
        for _ in 0..MAX_ATTEMPTS {
            let text = read_text(path).map_err(EditError::Io)?;
            let mut doc = parse_text(&text).map_err(EditError::Parse)?;
            let out = apply(&mut doc).map_err(EditError::Apply)?;
            let candidate = doc.to_string();
            validate(&candidate).map_err(EditError::Validate)?;
            if read_text(path).map_err(EditError::Io)? != text {
                continue;
            }
            write_document(path, &doc).map_err(EditError::Io)?;
            return Ok(out);
        }
        Err(EditError::ConcurrentModification)
    })();
    let _ = FileExt::unlock(&lock);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_run_write_never_replaces_an_existing_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        assert!(matches!(
            create_new(&path, "default_model = \"a/m\"\n"),
            Ok(())
        ));
        assert!(matches!(
            create_new(&path, "default_model = \"b/m\"\n"),
            Err(CreateNewError::AlreadyExists)
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("a/m") && !text.contains("b/m"), "{text}");
    }

    /// Two processes racing to create the same first-run file: the lock makes
    /// exactly one win and the other reports `AlreadyExists`; the winner's
    /// document is what remains, never a blend or a last-writer overwrite.
    #[test]
    fn two_concurrent_first_run_writes_produce_one_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let first = {
                let path = path.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    create_new(&path, "default_model = \"a/m\"\n")
                })
            };
            let second = {
                let path = path.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    create_new(&path, "default_model = \"b/m\"\n")
                })
            };
            (first.join().unwrap(), second.join().unwrap())
        });
        let wins = [results.0.is_ok(), results.1.is_ok()]
            .into_iter()
            .filter(|won| *won)
            .count();
        let refusals = [
            matches!(&results.0, Err(CreateNewError::AlreadyExists)),
            matches!(&results.1, Err(CreateNewError::AlreadyExists)),
        ]
        .into_iter()
        .filter(|refused| *refused)
        .count();
        assert_eq!((wins, refusals), (1, 1), "{results:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            (text.contains("a/m") && !text.contains("b/m"))
                || (text.contains("b/m") && !text.contains("a/m")),
            "{text}"
        );
    }

    #[test]
    fn a_writer_that_bypasses_the_lock_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "first = 1\n").unwrap();
        let mut interloper = true;
        edit(
            &path,
            |doc| {
                doc["ours"] = toml_edit::value(true);
                Ok::<_, ()>(())
            },
            |candidate| {
                if interloper {
                    interloper = false;
                    // Simulate a writer that does not take the advisory lock
                    // between this edit's read and its write.
                    std::fs::write(&path, "first = 1\ntheirs = 2\n").unwrap();
                }
                assert!(candidate.contains("ours = true"));
                Ok(())
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("theirs = 2"), "{text}");
        assert!(text.contains("ours = true"), "{text}");
        assert!(text.contains("first = 1"), "{text}");
    }

    #[test]
    fn an_edit_waits_for_the_exclusive_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "base = true\n").unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path(&path))
            .unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        let waiting = std::thread::spawn({
            let path = path.clone();
            move || {
                edit(
                    &path,
                    |doc| {
                        doc["waited"] = toml_edit::value(true);
                        Ok::<_, ()>(())
                    },
                    |_| Ok(()),
                )
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("waited"),
            "the edit must not write while another process holds the lock"
        );
        FileExt::unlock(&lock).unwrap();
        waiting.join().unwrap().unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("waited = true")
        );
    }

    #[test]
    fn an_invalid_candidate_never_replaces_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "kept = true\n").unwrap();
        let result = edit(
            &path,
            |doc| {
                doc["outcome"] = toml_edit::value("bad");
                Ok::<_, &'static str>(())
            },
            |_| Err("rejected"),
        );
        assert!(matches!(result, Err(EditError::Validate("rejected"))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept = true\n");
    }
}
