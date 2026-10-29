//! A compact repository map: which files exist, grouped and bounded, so a model
//! can orient without reading everything (spec §26.3 first-stage strategy).

use std::path::Path;

const IGNORED: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "vendor",
    ".leveler",
    "build",
    ".venv",
];
const MAX_FILES: usize = 400;

/// A lightweight repository map.
#[derive(Debug, Clone, Default)]
pub struct RepositoryMap {
    /// Workspace-relative file paths, sorted.
    pub files: Vec<String>,
    /// Whether the file list was truncated.
    pub truncated: bool,
}

impl RepositoryMap {
    /// Build a map by walking `root`, skipping common build/vendor directories.
    pub fn build(root: &Path) -> Self {
        let mut files = Vec::new();
        walk(root, root, 0, 6, &mut files);
        files.sort();
        let truncated = files.len() > MAX_FILES;
        files.truncate(MAX_FILES);
        RepositoryMap { files, truncated }
    }

    /// Render the map as a newline-separated list for prompting.
    pub fn render(&self) -> String {
        let mut s = self.files.join("\n");
        if self.truncated {
            s.push_str("\n… [more files omitted]");
        }
        s
    }
}

fn walk(root: &Path, dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<String>) {
    if depth > max_depth || out.len() > MAX_FILES * 2 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && name != ".leveler" || IGNORED.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, depth + 1, max_depth, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(slashes(rel));
        }
    }
}

/// Workspace-relative paths use `/` on every platform so prompts, `is_test`,
/// and scope checks match on one form (`strip_prefix` yields `\` on Windows).
fn slashes(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether a path looks like a source file worth considering.
pub fn is_source(path: &str) -> bool {
    matches!(
        extension(path),
        Some("rs")
            | Some("go")
            | Some("ts")
            | Some("tsx")
            | Some("js")
            | Some("jsx")
            | Some("py")
            | Some("java")
            | Some("rb")
            | Some("cs")
            | Some("cpp")
            | Some("cc")
            | Some("cxx")
            | Some("c")
            | Some("h")
            | Some("hpp")
            | Some("php")
            | Some("kt")
            | Some("swift")
    )
}

fn extension(path: &str) -> Option<&str> {
    path.rsplit('.').next().filter(|e| *e != path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_sources() {
        assert!(is_source("src/main.rs"));
        assert!(is_source("pkg/x.go"));
        assert!(!is_source("README.md"));
    }

    #[test]
    fn builds_map_skipping_ignored() {
        let dir =
            std::env::temp_dir().join(format!("leveler-map-{}", std::process::id() as u64 + 41));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("target/junk"), "").unwrap();
        let map = RepositoryMap::build(&dir);
        assert!(map.files.iter().any(|f| f == "src/main.rs"));
        assert!(!map.files.iter().any(|f| f.contains("target")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_includes_truncation_marker() {
        let map = RepositoryMap {
            files: vec!["a.rs".to_string(), "b.rs".to_string()],
            truncated: true,
        };
        assert_eq!(map.render(), "a.rs\nb.rs\n… [more files omitted]");
    }

    #[test]
    fn extension_returns_none_when_missing() {
        assert_eq!(extension("Makefile"), None);
        assert_eq!(extension("src/main"), None);
    }

    #[test]
    fn extension_returns_last_component() {
        assert_eq!(extension("src/main.rs"), Some("rs"));
        assert_eq!(extension("archive.tar.gz"), Some("gz"));
    }
}
