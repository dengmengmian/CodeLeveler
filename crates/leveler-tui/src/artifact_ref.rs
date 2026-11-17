//! References the transcript holds to content that lives on disk.
//!
//! A tool result above the model's output budget is saved by the runtime and
//! its preview starts with a reference line naming the absolute path. A local
//! command's complete capture names its directory in the same way. Both are
//! untrusted strings until they are resolved against the runtime's artifact
//! store and proven to be regular, content-addressed files: the model, and
//! therefore a tool result, controls the path text, while the runtime owns
//! where those files may live.

use std::path::{Path, PathBuf};

use crate::output_view::Capture;
use crate::state::AppState;

/// The absolute artifact path from a saved-output reference line.
pub fn saved_tool_output_path(preview: &str) -> Option<String> {
    let line = preview.lines().next()?;
    let rest = line.strip_prefix(leveler_model::tool_output::TOOL_OUTPUT_SAVED_PREFIX)?;
    // The runtime's reference is `… {path} | sha256:{hash} | {n} bytes. …`.
    // Requiring the digest separator keeps a tool result that merely begins
    // with the prefix from being read as a path.
    let (path, _) = rest.split_once(" | sha256:")?;
    let path = path.trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// An artifact is a content-addressed file in this runtime's artifact store.
/// Model output is untrusted: a path it names is evidence, not authority, so a
/// fake locator cannot point the client at a session database, a capture, the
/// memory store, or any other file the runtime happens to write nearby.
pub fn validated_tool_output(state: &AppState, path: &str) -> Option<PathBuf> {
    validated_artifact(state, Path::new(path))
}

/// Re-check a path the search corpus validated earlier. Used at the moment a
/// result opens, so a file swept (or a symlink swapped) after the scan is
/// never followed.
pub fn validated_file_under_state(state: &AppState, path: &Path) -> Option<PathBuf> {
    validated_artifact(state, path)
}

/// A capture directory is validated by [`Capture::open`] against the local
/// shell-output root, which also proves every stream copy is complete.
pub fn validated_shell_capture(state: &AppState, directory: &str) -> Result<Capture, String> {
    Capture::open(Path::new(directory), state.shell_output_root().as_deref())
}

/// Re-open the capture a search result points at, and require it to resolve
/// to the SAME file the scan read. A swept or replaced capture is refused
/// rather than silently opening different bytes.
pub fn revalidated_shell_capture(state: &AppState, path: &Path) -> Option<PathBuf> {
    let directory = path.parent()?;
    let capture = Capture::open(directory, state.shell_output_root().as_deref()).ok()?;
    let current = std::fs::canonicalize(&capture.combined).ok()?;
    let requested = std::fs::canonicalize(path).ok()?;
    (current == requested).then_some(current)
}

fn artifact_root(state: &AppState) -> Option<PathBuf> {
    let shell_root = state.shell_output_root()?;
    Some(shell_root.parent()?.join("artifacts"))
}

fn validated_artifact(state: &AppState, path: &Path) -> Option<PathBuf> {
    let root = artifact_root(state)?;
    let path = validated_file_under(&root, path)?;
    is_content_addressed(&path).then_some(path)
}

fn validated_file_under(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    let path = std::fs::canonicalize(path).ok()?;
    (path.is_file() && path.starts_with(&root)).then_some(path)
}

/// The artifact store writes `<sha256>.txt`. A different name in the same
/// directory is not a store artifact and is never opened from a transcript
/// reference.
fn is_content_addressed(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(hash) = name.strip_suffix(".txt") else {
        return false;
    };
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Boot;
    use leveler_client_protocol::SessionId;

    fn state(root: &Path) -> AppState {
        AppState::new(
            crate::theme::Theme::no_color(),
            Boot {
                session_id: SessionId::new("s1"),
                user: "u".into(),
                version: "0.1.0".into(),
                show_welcome: false,
                draft_path: Some(root.join("state").join("draft.txt")),
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                model_notice: None,
                thinking: None,
            },
        )
    }

    #[test]
    fn a_saved_reference_line_yields_its_absolute_path() {
        let preview = "[tool output saved: /tmp/state/artifacts/ab.txt | sha256:abc | 900 bytes. The preview below is partial.]\nhead\ntail";
        assert_eq!(
            saved_tool_output_path(preview).as_deref(),
            Some("/tmp/state/artifacts/ab.txt")
        );
        assert_eq!(
            saved_tool_output_path("[tool output saved: ]"),
            None,
            "empty path is not a reference"
        );
        assert_eq!(saved_tool_output_path("ordinary tool output"), None);
    }

    #[test]
    fn only_regular_files_under_the_state_root_are_artifacts() {
        let root = std::env::temp_dir().join(format!("leveler-find-refs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("state").join("artifacts")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        let inside = root
            .join("state")
            .join("artifacts")
            .join("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.txt");
        std::fs::write(&inside, "full output").unwrap();
        let outside = root.join("elsewhere").join("out.txt");
        std::fs::write(&outside, "not ours").unwrap();
        let s = state(&root);

        assert_eq!(
            validated_tool_output(&s, inside.to_str().unwrap()),
            Some(std::fs::canonicalize(&inside).unwrap())
        );
        assert_eq!(
            validated_tool_output(&s, outside.to_str().unwrap()),
            None,
            "a file outside the state root is not an artifact"
        );
        assert_eq!(
            validated_tool_output(&s, root.join("state").join("artifacts").to_str().unwrap()),
            None,
            "a directory is not an artifact"
        );
        let fake = root.join("state").join("artifacts").join("not-a-hash.txt");
        std::fs::write(&fake, "not a store artifact").unwrap();
        assert_eq!(
            validated_tool_output(&s, fake.to_str().unwrap()),
            None,
            "only content-addressed store files are artifacts"
        );

        #[cfg(unix)]
        {
            let link = root.join("state").join("artifacts").join("link.txt");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert_eq!(
                validated_tool_output(&s, link.to_str().unwrap()),
                None,
                "a symlink resolves outside the root"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_capture_result_revalidates_to_the_exact_file_it_scanned() {
        let root = std::env::temp_dir().join(format!(
            "leveler-find-capture-recheck-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let directory = root
            .join("state")
            .join("shell-output")
            .join("s1")
            .join("cap");
        std::fs::create_dir_all(&directory).unwrap();
        for stream in ["stdout", "stderr", "combined"] {
            std::fs::write(directory.join(format!("{stream}.log")), "line\n").unwrap();
            std::fs::write(
                directory.join(format!("{stream}.meta.json")),
                "{\"complete\":true}\n",
            )
            .unwrap();
        }
        let s = state(&root);
        let capture = validated_shell_capture(&s, directory.to_str().unwrap()).unwrap();
        assert_eq!(
            revalidated_shell_capture(&s, &capture.combined),
            Some(std::fs::canonicalize(&capture.combined).unwrap())
        );
        std::fs::remove_file(directory.join("combined.log")).unwrap();
        assert_eq!(
            revalidated_shell_capture(&s, &capture.combined),
            None,
            "a swept capture is not reopened as different bytes"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_capture_directory_must_be_complete_and_under_the_shell_root() {
        let root =
            std::env::temp_dir().join(format!("leveler-find-capture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let directory = root
            .join("state")
            .join("shell-output")
            .join("s1")
            .join("cap");
        std::fs::create_dir_all(&directory).unwrap();
        for stream in ["stdout", "stderr", "combined"] {
            std::fs::write(directory.join(format!("{stream}.log")), "line\n").unwrap();
            std::fs::write(
                directory.join(format!("{stream}.meta.json")),
                "{\"complete\":true}\n",
            )
            .unwrap();
        }
        let s = state(&root);
        let capture =
            validated_shell_capture(&s, directory.to_str().unwrap()).expect("complete capture");
        assert!(
            capture
                .combined
                .starts_with(std::fs::canonicalize(&directory).unwrap())
        );

        std::fs::write(
            directory.join("combined.meta.json"),
            "{\"complete\":false}\n",
        )
        .unwrap();
        assert!(
            validated_shell_capture(&s, directory.to_str().unwrap()).is_err(),
            "an unfinished capture is not complete output"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
