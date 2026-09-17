//! `WorkspaceSearch` — one deterministic traversal, ignore rule set and match
//! semantics beneath `list_files`, `find_files` and `grep`.
//!
//! The point of this module is that a call means the same thing on every
//! machine. Nothing here shells out: no `git ls-files`, no `rg`, no `fd`. The
//! candidate universe, the glob dialect and the regex dialect are the same on
//! macOS, Linux and Windows (`docs/ARCHITECTURE.md` §18.3 B).

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

/// Directories never worth walking. Build output and VCS internals are noise
/// in every repository, and `.git` in particular must never be enumerated.
const NEVER_WALK: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "vendor",
    ".leveler",
];

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirEntry {
    /// The entry's own name, with no path components.
    pub(crate) name: String,
    pub(crate) is_dir: bool,
}

impl DirEntry {
    /// The name as the model sees it: directories carry a trailing slash.
    pub(crate) fn display(&self) -> String {
        if self.is_dir {
            format!("{}/", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// The direct children of one directory.
#[derive(Debug)]
pub(crate) struct Listing {
    pub(crate) entries: Vec<DirEntry>,
    /// Entries beyond `limit` that were not returned.
    pub(crate) omitted: usize,
}

/// A path found by `find`, relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SearchPath(pub String);

/// Paths matching a glob.
#[derive(Debug)]
pub(crate) struct FindResult {
    pub(crate) paths: Vec<SearchPath>,
    /// Matches beyond `limit` that were not returned.
    pub(crate) omitted: usize,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrepMatch {
    /// Workspace-relative where possible, absolute otherwise.
    pub(crate) path: String,
    /// 1-based line number.
    pub(crate) line: usize,
    pub(crate) text: String,
}

/// Matches, plus what the search could not read.
#[derive(Debug)]
pub(crate) struct GrepResult {
    pub(crate) matches: Vec<GrepMatch>,
    /// Whether the scan stopped because `limit` was reached.
    pub(crate) limited: bool,
    /// Files skipped because they are not UTF-8 text.
    pub(crate) skipped_binary: usize,
}

/// What the model asked `grep` for. Every field is explicit: nothing about
/// the query is inferred from the host.
#[derive(Debug, Clone)]
pub(crate) struct GrepQuery {
    pub(crate) pattern: String,
    /// Treat `pattern` as literal text rather than a regex.
    pub(crate) literal: bool,
    pub(crate) ignore_case: bool,
    /// Optional glob restricting which files are searched.
    pub(crate) include: Option<String>,
    pub(crate) limit: usize,
}

/// Mechanical reasons a search cannot run.
#[derive(Debug)]
pub(crate) enum SearchError {
    NotDirectory,
    /// The pattern is not a valid glob.
    InvalidGlob {
        pattern: String,
        message: String,
    },
    /// The pattern is not a valid regex.
    InvalidRegex {
        pattern: String,
        message: String,
    },
    Cancelled,
    Io(String),
}

pub(crate) struct WorkspaceSearch;

impl WorkspaceSearch {
    /// The direct children of `dir`, sorted deterministically.
    ///
    /// This is directory inspection, not repository search: it never descends,
    /// and it hides nothing. A build directory is a fact about the directory.
    pub(crate) fn list_dir(dir: &Path, limit: usize) -> Result<Listing, SearchError> {
        if !dir.is_dir() {
            return Err(SearchError::NotDirectory);
        }
        let read = std::fs::read_dir(dir).map_err(|e| SearchError::Io(e.to_string()))?;
        let mut entries: Vec<DirEntry> = read
            .flatten()
            .map(|entry| DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                // Follows symlinks, so a link to a directory reads as one. It
                // is still never descended: this lists one level.
                is_dir: entry.path().is_dir(),
            })
            .collect();
        sort_entries(&mut entries);
        let omitted = entries.len().saturating_sub(limit);
        entries.truncate(limit);
        Ok(Listing { entries, omitted })
    }

    /// Paths under `root` matching `pattern`, relative to `relative_to`.
    ///
    /// `pattern` is always a glob. A pattern with no `/` matches the file
    /// name at any depth; a pattern containing `/` matches the path relative
    /// to `root`. That is the gitignore/ripgrep rule, and it is the only rule.
    pub(crate) async fn find(
        root: &Path,
        relative_to: &Path,
        pattern: &str,
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<FindResult, SearchError> {
        if !root.is_dir() {
            return Err(SearchError::NotDirectory);
        }
        let matcher = compile_glob(pattern)?;
        let root = root.to_path_buf();
        let relative_to = relative_to.to_path_buf();
        let cancellation = cancellation.clone();
        blocking(move || {
            let mut paths = Vec::new();
            let mut omitted = 0usize;
            for path in walk(&root, &cancellation) {
                if !matcher.is_match(&path, &root) {
                    continue;
                }
                if paths.len() >= limit {
                    omitted += 1;
                    continue;
                }
                paths.push(SearchPath(display_path(&path, &relative_to)));
            }
            paths.sort();
            Ok(FindResult { paths, omitted })
        })
        .await
    }

    /// Lines under `root` matching `query`.
    ///
    /// `root` may be a file or a directory. The pattern is a regex unless
    /// `literal` is set; that choice belongs to the caller and is never made
    /// for it by what happens to be installed on the machine.
    pub(crate) async fn grep(
        root: &Path,
        relative_to: &Path,
        query: &GrepQuery,
        cancellation: &CancellationToken,
    ) -> Result<GrepResult, SearchError> {
        let expression = if query.literal {
            regex::escape(&query.pattern)
        } else {
            query.pattern.clone()
        };
        let regex = regex::RegexBuilder::new(&expression)
            .case_insensitive(query.ignore_case)
            .build()
            .map_err(|e| SearchError::InvalidRegex {
                pattern: query.pattern.clone(),
                message: first_line(&e.to_string()),
            })?;
        let include = query.include.as_deref().map(compile_glob).transpose()?;

        let root = root.to_path_buf();
        let relative_to = relative_to.to_path_buf();
        let limit = query.limit;
        let cancellation = cancellation.clone();
        blocking(move || {
            let files: Vec<PathBuf> = if root.is_file() {
                vec![root.clone()]
            } else {
                walk(&root, &cancellation)
            };
            let mut matches = Vec::new();
            let mut skipped_binary = 0usize;
            let mut limited = false;
            for file in files {
                if cancellation.is_cancelled() {
                    return Err(SearchError::Cancelled);
                }
                if let Some(include) = &include
                    && !include.is_match(&file, &root)
                {
                    continue;
                }
                match scan_file(&file, &regex, limit.saturating_sub(matches.len())) {
                    FileScan::Matches(found) => {
                        for (line, text) in found {
                            matches.push(GrepMatch {
                                path: display_path(&file, &relative_to),
                                line,
                                text,
                            });
                        }
                        if matches.len() >= limit {
                            limited = true;
                            break;
                        }
                    }
                    FileScan::NotText => skipped_binary += 1,
                    FileScan::Unreadable => {}
                }
            }
            Ok(GrepResult {
                matches,
                limited,
                skipped_binary,
            })
        })
        .await
    }
}

/// Case-insensitive, then exact, so `apple`/`Banana`/`Zebra` read naturally
/// while ties stay deterministic.
fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// A compiled glob plus the anchoring rule that applies to it.
struct GlobMatcher {
    glob: globset::GlobMatcher,
    /// Whether the pattern is matched against the relative path (it contains
    /// a separator) or against the file name.
    against_path: bool,
}

impl GlobMatcher {
    fn is_match(&self, path: &Path, root: &Path) -> bool {
        if self.against_path {
            let relative = path.strip_prefix(root).unwrap_or(path);
            self.glob.is_match(normalize(relative))
        } else {
            match path.file_name() {
                Some(name) => self.glob.is_match(name),
                None => false,
            }
        }
    }
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, SearchError> {
    if pattern.is_empty() {
        return Err(SearchError::InvalidGlob {
            pattern: pattern.to_string(),
            message: "pattern must not be empty".to_string(),
        });
    }
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| SearchError::InvalidGlob {
            pattern: pattern.to_string(),
            message: first_line(&e.to_string()),
        })?;
    Ok(GlobMatcher {
        glob: glob.compile_matcher(),
        against_path: pattern.contains('/'),
    })
}

/// Workspace paths use `/` on every platform so a model's pattern and the
/// results it gets back are written the same way.
fn normalize(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn display_path(path: &Path, relative_to: &Path) -> String {
    match path.strip_prefix(relative_to) {
        Ok(relative) => normalize(relative),
        Err(_) => normalize(path),
    }
}

fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).trim().to_string()
}

/// The one traversal. `.gitignore` and `.ignore` files are honoured whether or
/// not this is a Git repository and whether or not Git is installed; the
/// user's global gitignore is deliberately NOT read, because it would make the
/// same repository search differently on two machines.
fn walk(root: &Path, cancellation: &CancellationToken) -> Vec<PathBuf> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(false)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    builder.filter_entry(|entry| {
        entry
            .file_name()
            .to_str()
            .is_none_or(|name| !NEVER_WALK.contains(&name))
    });

    let mut paths = Vec::new();
    for entry in builder.build() {
        if cancellation.is_cancelled() {
            break;
        }
        let Ok(entry) = entry else { continue };
        // `follow_links(false)` already refuses to descend a symlinked
        // directory; this also keeps the link itself out of the results.
        if entry.file_type().is_some_and(|t| t.is_file()) {
            paths.push(entry.into_path());
        }
    }
    paths
}

enum FileScan {
    Matches(Vec<(usize, String)>),
    /// The file is not UTF-8 text, so it was not searched.
    NotText,
    Unreadable,
}

/// Scan one file line by line. Bounded in memory by the longest line, and it
/// stops the moment the file proves not to be text.
fn scan_file(path: &Path, regex: &regex::Regex, remaining: usize) -> FileScan {
    use std::io::BufRead;

    let Ok(file) = std::fs::File::open(path) else {
        return FileScan::Unreadable;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut raw = Vec::new();
    let mut found = Vec::new();
    let mut number = 0usize;
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return FileScan::Unreadable,
        }
        number += 1;
        if raw.contains(&0) {
            return FileScan::NotText;
        }
        let mut end = raw.as_slice();
        if end.ends_with(b"\n") {
            end = &end[..end.len() - 1];
        }
        if end.ends_with(b"\r") {
            end = &end[..end.len() - 1];
        }
        let Ok(text) = std::str::from_utf8(end) else {
            return FileScan::NotText;
        };
        if regex.is_match(text) {
            found.push((number, text.to_string()));
            if found.len() >= remaining {
                break;
            }
        }
    }
    FileScan::Matches(found)
}

/// Run a blocking traversal off the async runtime.
async fn blocking<T, F>(work: F) -> Result<T, SearchError>
where
    F: FnOnce() -> Result<T, SearchError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(e) => Err(SearchError::Io(format!("search task failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("leveler-search-")
            .tempdir()
            .unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/inner")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("src/inner/parser.rs"), "fn   main() {}\n").unwrap();
        std::fs::write(root.join("README.md"), "docs\n").unwrap();
        std::fs::write(root.join(".hidden"), "secret\n").unwrap();
        std::fs::write(root.join("target/debug/build.rs"), "fn main() {}\n").unwrap();
        dir
    }

    #[test]
    fn list_dir_is_one_level_and_hides_nothing() {
        let dir = fixture();
        let listing = WorkspaceSearch::list_dir(dir.path(), 100).unwrap();
        let names: Vec<String> = listing.entries.iter().map(DirEntry::display).collect();
        assert!(names.contains(&"src/".to_string()), "{names:?}");
        assert!(names.contains(&"target/".to_string()), "{names:?}");
        assert!(names.contains(&".hidden".to_string()), "{names:?}");
        assert!(
            !names
                .iter()
                .any(|n| n.contains('/') && n != "src/" && n != "target/"),
            "no nested paths: {names:?}"
        );
    }

    #[test]
    fn list_dir_sorting_is_stable_and_case_insensitive() {
        let dir = tempfile::Builder::new()
            .prefix("leveler-sort-")
            .tempdir()
            .unwrap();
        for name in ["Zebra", "apple", "Banana"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let listing = WorkspaceSearch::list_dir(dir.path(), 100).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["apple", "Banana", "Zebra"]);
    }

    #[tokio::test]
    async fn find_matches_a_bare_pattern_against_the_file_name() {
        let dir = fixture();
        let found = WorkspaceSearch::find(
            dir.path(),
            dir.path(),
            "parser.rs",
            100,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(found.paths, vec![SearchPath("src/inner/parser.rs".into())]);
    }

    #[tokio::test]
    async fn find_matches_a_slashed_pattern_against_the_relative_path() {
        let dir = fixture();
        let found = WorkspaceSearch::find(
            dir.path(),
            dir.path(),
            "src/**/*.rs",
            100,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            found.paths,
            vec![
                SearchPath("src/inner/parser.rs".into()),
                SearchPath("src/lib.rs".into())
            ]
        );
    }

    #[tokio::test]
    async fn find_does_not_treat_a_bare_word_as_a_substring() {
        let dir = fixture();
        let found = WorkspaceSearch::find(
            dir.path(),
            dir.path(),
            "pars",
            100,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(found.paths.is_empty(), "{:?}", found.paths);
    }

    #[tokio::test]
    async fn find_skips_build_output() {
        let dir = fixture();
        let found = WorkspaceSearch::find(
            dir.path(),
            dir.path(),
            "**/*.rs",
            100,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            !found.paths.iter().any(|p| p.0.starts_with("target/")),
            "{:?}",
            found.paths
        );
    }

    #[tokio::test]
    async fn find_reports_an_invalid_glob() {
        let dir = fixture();
        let err = WorkspaceSearch::find(
            dir.path(),
            dir.path(),
            "src/[",
            100,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SearchError::InvalidGlob { .. }), "{err:?}");
    }

    fn query(pattern: &str) -> GrepQuery {
        GrepQuery {
            pattern: pattern.to_string(),
            literal: false,
            ignore_case: false,
            include: None,
            limit: 100,
        }
    }

    #[tokio::test]
    async fn grep_pattern_is_a_regex() {
        let dir = fixture();
        let result = WorkspaceSearch::grep(
            dir.path(),
            dir.path(),
            &query("fn {2,}main"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let paths: Vec<&str> = result.matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["src/inner/parser.rs"]);
    }

    #[tokio::test]
    async fn grep_literal_mode_escapes_the_pattern() {
        let dir = fixture();
        let mut q = query("fn {2,}main");
        q.literal = true;
        let result = WorkspaceSearch::grep(dir.path(), dir.path(), &q, &CancellationToken::new())
            .await
            .unwrap();
        assert!(result.matches.is_empty(), "{:?}", result.matches);
    }

    #[tokio::test]
    async fn grep_reports_an_invalid_regex() {
        let dir = fixture();
        let err = WorkspaceSearch::grep(
            dir.path(),
            dir.path(),
            &query("a(b"),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SearchError::InvalidRegex { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn grep_include_filters_by_glob() {
        let dir = fixture();
        let mut q = query("\\w+");
        q.include = Some("*.md".to_string());
        let result = WorkspaceSearch::grep(dir.path(), dir.path(), &q, &CancellationToken::new())
            .await
            .unwrap();
        let paths: Vec<&str> = result.matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["README.md"]);
    }

    #[tokio::test]
    async fn grep_skips_files_that_are_not_text() {
        let dir = tempfile::Builder::new()
            .prefix("leveler-grep-bin-")
            .tempdir()
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("b.bin"), b"needle\x00\x01\n").unwrap();
        let result = WorkspaceSearch::grep(
            dir.path(),
            dir.path(),
            &query("needle"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.skipped_binary, 1);
    }

    #[tokio::test]
    async fn grep_honours_gitignore_without_git_installed() {
        let dir = tempfile::Builder::new()
            .prefix("leveler-grep-ignore-")
            .tempdir()
            .unwrap();
        std::fs::write(dir.path().join(".gitignore"), "generated.rs\n").unwrap();
        std::fs::write(dir.path().join("generated.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "needle\n").unwrap();
        let result = WorkspaceSearch::grep(
            dir.path(),
            dir.path(),
            &query("needle"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let paths: Vec<&str> = result.matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(paths, vec!["kept.rs"]);
    }
}
