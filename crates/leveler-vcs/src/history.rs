//! Read-only access to a repository's committed object database without
//! starting a `git` process.
//!
//! This exists for hosts where a per-command network denial cannot be
//! enforced. A `git` process — even for `show`/`log` — may lazily fetch a
//! missing object from a partial clone's promisor remote, run a configured
//! helper, or consult credentials. Reading loose objects and packfiles with
//! ordinary file I/O cannot reach the network, run repository code, or write
//! repository metadata. Every object is re-hashed before use, so a corrupt or
//! substituted object is refused instead of rendered as evidence.
//!
//! The reader is deliberately bounded: object size, delta depth, history
//! depth, search matches and rendered output all have caps. A limit is
//! reported, never silently crossed.

use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, FixedOffset};
use sha1::{Digest, Sha1};

/// Decompressed object cap. A larger object is refused: the tool output cap is
/// far below this, and an unbounded read is a memory-exhaustion primitive.
const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
/// Nested delta cap. Real Git limits delta chains to 50.
const MAX_DELTA_DEPTH: usize = 64;
/// Rendered output cap, matching the process-output cap the git path uses.
pub const OUTPUT_CAP: usize = 1024 * 1024;
/// Search result cap.
const MAX_SEARCH_MATCHES: usize = 1000;
/// Walk cap for one revision's tree.
const MAX_TREE_ENTRIES: usize = 500_000;
/// Cap on changed files one object diff may enumerate.
const MAX_CHANGED_FILES: usize = 100_000;
/// Blame history cap.
const MAX_BLAME_COMMITS: usize = 4000;
/// Ancestry search cap.
const MAX_ANCESTRY_COMMITS: usize = 1_000_000;
/// Largest line count a line diff will align. Above it the renderer states a
/// whole-file replacement instead of pretending the inner edit is known.
const MAX_DIFF_CELLS: usize = 4_000_000;

/// Errors from the read-only object reader.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("not a git repository: {0}")]
    NotARepository(String),
    #[error("unsupported repository feature: {0}")]
    Unsupported(String),
    #[error("invalid revision: {0}")]
    InvalidRevision(String),
    #[error("object {0} is missing from the local object database (network fetch is disabled)")]
    ObjectMissing(String),
    #[error("corrupt object {0}: {1}")]
    Corrupt(String, String),
    #[error("path is not valid in this repository: {0}")]
    InvalidPath(String),
    #[error("query exceeded its bound: {0}")]
    Limit(String),
    #[error("git object io error: {0}")]
    Io(#[from] std::io::Error),
}

type Oid = [u8; 20];

enum RefValue {
    Oid(Oid),
    Symbolic(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectKind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjectKind {
    fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Commit => "commit",
            ObjectKind::Tree => "tree",
            ObjectKind::Blob => "blob",
            ObjectKind::Tag => "tag",
        }
    }
}

fn oid_hex(oid: &Oid) -> String {
    let mut text = String::with_capacity(40);
    for byte in oid {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn parse_oid(text: &str) -> Option<Oid> {
    if text.len() != 40 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut oid = [0u8; 20];
    for (index, byte) in oid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(oid)
}

#[derive(Debug, Clone)]
struct Signature {
    name: String,
    email: String,
    seconds: i64,
    offset_minutes: i32,
}

impl Signature {
    fn iso8601(&self) -> String {
        let offset = FixedOffset::east_opt(self.offset_minutes * 60)
            .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
        DateTime::from_timestamp(self.seconds, 0)
            .map(|utc| {
                utc.with_timezone(&offset)
                    .format("%Y-%m-%dT%H:%M:%S%:z")
                    .to_string()
            })
            .unwrap_or_else(|| self.seconds.to_string())
    }

    fn human(&self) -> String {
        let offset = FixedOffset::east_opt(self.offset_minutes * 60)
            .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
        DateTime::from_timestamp(self.seconds, 0)
            .map(|utc| {
                utc.with_timezone(&offset)
                    .format("%Y-%m-%d %H:%M:%S %z")
                    .to_string()
            })
            .unwrap_or_else(|| self.seconds.to_string())
    }
}

#[derive(Debug, Clone)]
struct Commit {
    oid: Oid,
    tree: Oid,
    parents: Vec<Oid>,
    author: Signature,
    committer: Signature,
    message: String,
}

impl Commit {
    fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

#[derive(Debug, Clone)]
struct TreeEntry {
    mode: u32,
    name: Vec<u8>,
    oid: Oid,
}

impl TreeEntry {
    fn is_tree(&self) -> bool {
        self.mode & 0o170000 == 0o040000
    }

    fn is_gitlink(&self) -> bool {
        self.mode & 0o170000 == 0o160000
    }

    fn is_symlink(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }
}

fn parse_signature(raw: &str) -> Signature {
    // `Name <email> 1699999999 +0800`
    let mut rest = raw.trim();
    let mut offset_minutes = 0;
    let mut seconds = 0;
    if let Some((before, zone)) = rest.rsplit_once(' ')
        && let Some((before, time)) = before.rsplit_once(' ')
        && let Ok(value) = time.parse::<i64>()
    {
        seconds = value;
        offset_minutes = parse_offset(zone);
        rest = before.trim();
    }
    let (name, email) = match rest.rsplit_once(" <") {
        Some((name, email)) if email.ends_with('>') => (
            name.trim().to_string(),
            email[..email.len() - 1].to_string(),
        ),
        _ => (rest.to_string(), String::new()),
    };
    Signature {
        name,
        email,
        seconds,
        offset_minutes,
    }
}

fn parse_offset(zone: &str) -> i32 {
    let bytes = zone.as_bytes();
    if bytes.len() != 5 || (bytes[0] != b'+' && bytes[0] != b'-') {
        return 0;
    }
    let (Ok(hours), Ok(minutes)) = (zone[1..3].parse::<i32>(), zone[3..5].parse::<i32>()) else {
        return 0;
    };
    let total = hours * 60 + minutes;
    if bytes[0] == b'-' { -total } else { total }
}

/// One parsed commit, as the renderers need it.
#[derive(Debug, Clone)]
pub struct CommitView {
    pub oid: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_time: String,
    pub committer_time: String,
    pub message: String,
}

/// The read-only view of one repository's committed objects.
pub struct RepositoryHistory {
    repo_root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    packed: BTreeMap<String, Oid>,
    shallow: HashSet<Oid>,
    object_dirs: Vec<ObjectDir>,
    cache: Mutex<ObjectCache>,
}

struct ObjectDir {
    root: PathBuf,
    packs: Vec<PackIndex>,
}

struct PackIndex {
    oids: Vec<Oid>,
    offsets: Vec<u64>,
    pack_path: PathBuf,
}

#[derive(Default)]
struct ObjectCache {
    entries: HashMap<Oid, (ObjectKind, Arc<Vec<u8>>)>,
    bytes: usize,
}

impl RepositoryHistory {
    /// Open the repository containing `workspace`.
    ///
    /// Only the committed object database is read. A worktree's `HEAD` is
    /// per-checkout; refs and objects live in the common directory.
    pub fn open(workspace: &Path) -> Result<Self, HistoryError> {
        let repo_root = discover_repo_root(workspace)?;
        let dot_git = repo_root.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else {
            let text = std::fs::read_to_string(&dot_git)
                .map_err(|_| HistoryError::NotARepository(workspace.display().to_string()))?;
            let target = text
                .lines()
                .find_map(|line| line.strip_prefix("gitdir:"))
                .map(str::trim)
                .ok_or_else(|| HistoryError::NotARepository(workspace.display().to_string()))?;
            let target = Path::new(target);
            if target.is_absolute() {
                target.to_path_buf()
            } else {
                repo_root.join(target)
            }
        };
        let git_dir = std::fs::canonicalize(&git_dir)?;
        let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
            Ok(text) => {
                let path = Path::new(text.trim());
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    git_dir.join(path)
                };
                std::fs::canonicalize(path)?
            }
            Err(_) => git_dir.clone(),
        };

        check_repository_format(&common_dir)?;
        let packed = read_packed_refs(&common_dir)?;
        // A replace ref rewrites an object's content on read. The reader does
        // not implement that mapping, and silently ignoring it would present
        // different evidence than the repository's own Git view.
        if packed.keys().any(|name| name.starts_with("refs/replace/"))
            || common_dir.join("refs/replace").is_dir()
            || std::fs::read_to_string(common_dir.join("info/grafts"))
                .is_ok_and(|text| !text.trim().is_empty())
        {
            return Err(HistoryError::Unsupported(
                "replace refs and grafts are not supported by the local reader".into(),
            ));
        }
        let shallow = read_shallow(&common_dir)?;
        let object_dirs = collect_object_dirs(&common_dir.join("objects"))?;

        Ok(Self {
            repo_root,
            git_dir,
            common_dir,
            packed,
            shallow,
            object_dirs,
            cache: Mutex::new(ObjectCache::default()),
        })
    }

    /// The repository root (the directory holding `.git`).
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Resolve `HEAD` to a commit, or `None` when the repository has no commit.
    pub fn head(&self) -> Result<Option<String>, HistoryError> {
        match self.resolve_commit("HEAD") {
            Ok(oid) => Ok(Some(oid_hex(&oid))),
            Err(HistoryError::InvalidRevision(_)) | Err(HistoryError::ObjectMissing(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Resolve any supported revision expression to a commit id.
    pub fn resolve_commit_id(&self, rev: &str) -> Result<String, HistoryError> {
        self.resolve_commit(rev).map(|oid| oid_hex(&oid))
    }

    /// Metadata for one commit-ish.
    pub fn commit(&self, rev: &str) -> Result<CommitView, HistoryError> {
        let oid = self.resolve_commit(rev)?;
        let commit = self.read_commit(&oid)?;
        Ok(commit_view(&commit))
    }

    /// Whether `ancestor` is reachable from `descendant` through first and
    /// later parents.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, HistoryError> {
        let ancestor = self.resolve_commit(ancestor)?;
        let descendant = self.resolve_commit(descendant)?;
        let mut queue = VecDeque::from([descendant]);
        let mut seen = HashSet::new();
        while let Some(oid) = queue.pop_front() {
            if oid == ancestor {
                return Ok(true);
            }
            if !seen.insert(oid) {
                continue;
            }
            if seen.len() > MAX_ANCESTRY_COMMITS {
                return Err(HistoryError::Limit(format!(
                    "ancestry walk exceeded {MAX_ANCESTRY_COMMITS} commits"
                )));
            }
            for parent in self.parents_of(&oid)? {
                queue.push_back(parent);
            }
        }
        Ok(false)
    }

    /// The best common ancestor of two commit-ish revisions.
    ///
    /// The candidate set is the intersection of both ancestry closures; the
    /// newest committer timestamp wins (git's merge-base chooses a "best"
    /// common ancestor, and on the histories reviews use this is that one).
    /// An unrelated history is reported instead of falling back to a tree
    /// diff against the wrong base.
    pub fn merge_base(&self, left: &str, right: &str) -> Result<Option<String>, HistoryError> {
        let left = self.resolve_commit(left)?;
        let right = self.resolve_commit(right)?;
        let mut left_ancestors = HashSet::new();
        let mut queue = VecDeque::from([left]);
        while let Some(oid) = queue.pop_front() {
            if !left_ancestors.insert(oid) {
                continue;
            }
            if left_ancestors.len() > MAX_ANCESTRY_COMMITS {
                return Err(HistoryError::Limit(format!(
                    "ancestry walk exceeded {MAX_ANCESTRY_COMMITS} commits"
                )));
            }
            for parent in self.parents_of(&oid)? {
                queue.push_back(parent);
            }
        }
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([right]);
        let mut best: Option<(i64, Oid)> = None;
        while let Some(oid) = queue.pop_front() {
            if !seen.insert(oid) {
                continue;
            }
            if seen.len() > MAX_ANCESTRY_COMMITS {
                return Err(HistoryError::Limit(format!(
                    "ancestry walk exceeded {MAX_ANCESTRY_COMMITS} commits"
                )));
            }
            if left_ancestors.contains(&oid) {
                let commit = self.read_commit(&oid)?;
                let candidate = (commit.committer.seconds, oid);
                if best.is_none_or(|best| candidate > best) {
                    best = Some(candidate);
                }
            }
            for parent in self.parents_of(&oid)? {
                queue.push_back(parent);
            }
        }
        Ok(best.map(|(_, oid)| oid_hex(&oid)))
    }

    /// `git log --format=%H%x09%aI%x09%an%x09%s`.
    pub fn log(
        &self,
        rev: &str,
        max_count: u32,
        path: Option<&str>,
    ) -> Result<String, HistoryError> {
        let tip = self.resolve_commit(rev)?;
        let path = path.map(|path| path.as_bytes().to_vec());
        let mut queue = BinaryHeap::new();
        queue.push(self.commit_sort_key(&tip)?);
        let mut seen = HashSet::new();
        let mut lines = Vec::new();
        while let Some(key) = queue.pop() {
            if !seen.insert(key.1) {
                continue;
            }
            let commit = self.read_commit(&key.1)?;
            let include = match &path {
                None => true,
                Some(path) => self.commit_changes_path(&commit, path)?,
            };
            if include {
                lines.push(format!(
                    "{}\t{}\t{}\t{}",
                    oid_hex(&commit.oid),
                    commit.author.iso8601(),
                    commit.author.name.replace(['\t', '\n'], " "),
                    commit.subject().replace(['\t', '\n'], " ")
                ));
                if lines.len() >= max_count as usize {
                    break;
                }
            }
            for parent in self.effective_parents(&commit) {
                queue.push(self.commit_sort_key(&parent)?);
            }
        }
        Ok(lines.join("\n"))
    }

    /// One commit's metadata, stat and patch, in `git show` shape.
    ///
    /// A merge is rendered against its first parent and says so: `git show`'s
    /// default combined diff is empty for a clean merge even though the commit
    /// carries the branch's changes.
    pub fn show(&self, rev: &str, path: Option<&str>) -> Result<String, HistoryError> {
        let oid = self.resolve_commit(rev)?;
        let commit = self.read_commit(&oid)?;
        let mut body = render_commit_header(&commit);
        let parents = self.effective_parents(&commit);
        if parents.len() > 1 {
            body.push_str(&format!(
                "\n[note] merge commit: this diff is against the first parent ({}^{}), not a combined diff.\n\n",
                oid_hex(&commit.oid),
                1
            ));
        }
        let parent_tree = match parents.first() {
            Some(parent) => Some(self.read_commit(parent)?.tree),
            None => None,
        };
        let changes = self.diff_trees(parent_tree.as_ref(), Some(&commit.tree), path)?;
        body.push_str(&self.render_stat(&changes)?);
        body.push('\n');
        body.push_str(&self.render_changes(&changes)?);
        Ok(cap_output(body))
    }

    /// Exact file content at a revision.
    pub fn file(&self, rev: &str, path: &str) -> Result<String, HistoryError> {
        let oid = self.resolve_commit(rev)?;
        let commit = self.read_commit(&oid)?;
        let entry = self.lookup_path(&commit.tree, path)?.ok_or_else(|| {
            HistoryError::InvalidPath(format!("`{path}` does not exist at this revision"))
        })?;
        if entry.is_tree() {
            return Err(HistoryError::InvalidPath(format!(
                "`{path}` is a directory"
            )));
        }
        if entry.is_gitlink() {
            return Err(HistoryError::InvalidPath(format!(
                "`{path}` is a submodule, not a file in this object database"
            )));
        }
        let (kind, data) = self.read_object(&entry.oid)?;
        if kind != ObjectKind::Blob {
            return Err(HistoryError::Corrupt(
                oid_hex(&entry.oid),
                "tree entry does not name a blob".into(),
            ));
        }
        Ok(cap_output(String::from_utf8_lossy(&data).into_owned()))
    }

    /// Repository-relative paths in a revision's tree.
    pub fn list(&self, rev: &str, path: Option<&str>) -> Result<String, HistoryError> {
        let oid = self.resolve_commit(rev)?;
        let commit = self.read_commit(&oid)?;
        let mut entries = Vec::new();
        let base = path.map(|path| path.trim_end_matches('/').to_string());
        let mut budget = MAX_TREE_ENTRIES;
        self.walk_tree(
            &commit.tree,
            "",
            0,
            &mut budget,
            &mut |entry_path, entry| {
                if !path_matches(entry_path, base.as_deref()) {
                    return;
                }
                if !entry.is_tree() {
                    entries.push(entry_path.to_string());
                }
            },
        )?;
        entries.sort();
        Ok(cap_output(entries.join("\n")))
    }

    /// Search a revision's tree with a basic regular expression.
    ///
    /// Output has the shape `git grep -n` produces for a tree-ish:
    /// `<rev>:<path>:<line>:<text>`.
    pub fn search(
        &self,
        rev: &str,
        pattern: &str,
        path: Option<&str>,
    ) -> Result<String, HistoryError> {
        let expression = basic_regex(pattern)?;
        let oid = self.resolve_commit(rev)?;
        let commit = self.read_commit(&oid)?;
        let base = path.map(|path| path.trim_end_matches('/').to_string());
        let mut output = Vec::new();
        let mut limit_hit = false;
        let mut budget = MAX_TREE_ENTRIES;
        self.walk_tree(
            &commit.tree,
            "",
            0,
            &mut budget,
            &mut |entry_path, entry| {
                if limit_hit || entry.is_tree() || entry.is_gitlink() {
                    return;
                }
                if !path_matches(entry_path, base.as_deref()) {
                    return;
                }
                let Ok((kind, data)) = self.read_object(&entry.oid) else {
                    return;
                };
                if kind != ObjectKind::Blob || data.len() > 8 * 1024 * 1024 {
                    return;
                }
                for (index, line) in split_lines(&data).into_iter().enumerate() {
                    let line = line.strip_suffix(b"\n").unwrap_or(line);
                    if expression.is_match(line) {
                        output.push(format!(
                            "{rev}:{entry_path}:{}:{}",
                            index + 1,
                            String::from_utf8_lossy(line)
                        ));
                        if output.len() >= MAX_SEARCH_MATCHES {
                            limit_hit = true;
                            return;
                        }
                    }
                }
            },
        )?;
        if limit_hit {
            output.push(format!(
                "[note] search stopped after {MAX_SEARCH_MATCHES} matches; narrow the pattern or path."
            ));
        }
        Ok(output.join("\n"))
    }

    /// Line attribution for one file, following first-parent history.
    ///
    /// Merge commits are attributed when they introduce or change a line;
    /// unchanged lines pass to the first parent. Rename following and
    /// `--find-copies` are intentionally not implemented, so the output says
    /// which model produced it.
    pub fn blame(
        &self,
        rev: &str,
        path: &str,
        line_start: Option<u32>,
        line_end: Option<u32>,
    ) -> Result<String, HistoryError> {
        let tip = self.resolve_commit(rev)?;
        let tip_commit = self.read_commit(&tip)?;
        let entry = self.lookup_path(&tip_commit.tree, path)?.ok_or_else(|| {
            HistoryError::InvalidPath(format!("`{path}` does not exist at this revision"))
        })?;
        if entry.is_tree() || entry.is_gitlink() {
            return Err(HistoryError::InvalidPath(format!(
                "`{path}` is not a file at this revision"
            )));
        }
        let (_, target_blob) = self.read_object(&entry.oid)?;
        let target_lines = split_lines(&target_blob);
        let total = target_lines.len();
        let (from, to) = match (line_start, line_end) {
            (Some(start), Some(end)) if start >= 1 && end >= start => {
                (start as usize, (end as usize).min(total))
            }
            _ => (1, total),
        };
        // Each target line carries the commit and 1-based line it currently
        // maps to while history is walked backwards.
        let mut origins: Vec<(Oid, usize)> = (1..=total).map(|line| (tip, line)).collect();
        let mut remaining: Vec<usize> = (0..total).collect();
        let mut current = tip;
        let mut merge_seen = false;
        for _ in 0..MAX_BLAME_COMMITS {
            if remaining.is_empty() {
                break;
            }
            let commit = self.read_commit(&current)?;
            if commit.parents.len() > 1 {
                merge_seen = true;
            }
            let parents = self.effective_parents(&commit);
            let Some(parent) = parents.first().copied() else {
                for index in remaining.drain(..) {
                    origins[index] = (current, origins[index].1);
                }
                break;
            };
            let parent_commit = self.read_commit(&parent)?;
            let parent_entry = self.lookup_path(&parent_commit.tree, path)?;
            let Some(parent_entry) =
                parent_entry.filter(|entry| !entry.is_tree() && !entry.is_gitlink())
            else {
                for index in remaining.drain(..) {
                    origins[index] = (current, origins[index].1);
                }
                break;
            };
            let (_, current_blob) = {
                let current_entry = self
                    .lookup_path(&commit.tree, path)?
                    .filter(|entry| !entry.is_tree() && !entry.is_gitlink());
                let Some(current_entry) = current_entry else {
                    for index in remaining.drain(..) {
                        origins[index] = (current, origins[index].1);
                    }
                    break;
                };
                self.read_object(&current_entry.oid)?
            };
            let (_, parent_blob) = self.read_object(&parent_entry.oid)?;
            let ops = diff_ops(&parent_blob, &current_blob);
            let mapping = equal_line_mapping(&ops);
            let mut next_remaining = Vec::new();
            for index in remaining {
                let (_, line) = origins[index];
                match mapping.get(&(line - 1)) {
                    Some(parent_line) => {
                        origins[index] = (parent, parent_line + 1);
                        next_remaining.push(index);
                    }
                    None => origins[index] = (current, line),
                }
            }
            remaining = next_remaining;
            current = parent;
        }
        if !remaining.is_empty() {
            return Err(HistoryError::Limit(format!(
                "blame history exceeded {MAX_BLAME_COMMITS} commits"
            )));
        }
        let mut commits: HashMap<Oid, Commit> = HashMap::new();
        let mut rows = Vec::new();
        for (index, (commit_oid, line)) in origins.iter().enumerate() {
            if index + 1 < from || index + 1 > to {
                continue;
            }
            let commit = match commits.get(commit_oid) {
                Some(commit) => commit.clone(),
                None => {
                    let commit = self.read_commit(commit_oid)?;
                    commits.insert(*commit_oid, commit.clone());
                    commit
                }
            };
            let short: String = oid_hex(commit_oid).chars().take(12).collect();
            let content = String::from_utf8_lossy(
                target_lines[index]
                    .strip_suffix(b"\n")
                    .unwrap_or(target_lines[index]),
            );
            rows.push(format!(
                "{short} ({}\t{} {line}) {content}",
                commit.author.name,
                commit.author.iso8601(),
            ));
        }
        if merge_seen {
            rows.insert(
                0,
                "[note] blame follows first-parent history; merge commits are attributed when they \
                 change a line."
                    .to_string(),
            );
        }
        Ok(cap_output(rows.join("\n")))
    }

    /// Unified patch between two commit-ish revisions.
    pub fn diff(&self, from: &str, to: &str, path: Option<&str>) -> Result<String, HistoryError> {
        let from_tree = self.read_commit(&self.resolve_commit(from)?)?.tree;
        let to_tree = self.read_commit(&self.resolve_commit(to)?)?.tree;
        let changes = self.diff_trees(Some(&from_tree), Some(&to_tree), path)?;
        Ok(cap_output(self.render_changes(&changes)?))
    }

    /// Paths that differ between two commit-ish trees.
    pub fn changed_files(&self, from: &str, to: &str) -> Result<Vec<String>, HistoryError> {
        let from_tree = self.read_commit(&self.resolve_commit(from)?)?.tree;
        let to_tree = self.read_commit(&self.resolve_commit(to)?)?.tree;
        let changes = self.diff_trees(Some(&from_tree), Some(&to_tree), None)?;
        Ok(changes.into_iter().map(|change| change.path).collect())
    }

    /// Whether tracked worktree content differs from `HEAD`.
    ///
    /// This reads the worktree and `HEAD` tree only; it never runs
    /// `git status`. Untracked files are not scanned (they are not part of
    /// any commit), and CRLF is folded so an `autocrlf` checkout is not
    /// misreported as dirty.
    pub fn worktree_dirty(&self) -> Result<bool, HistoryError> {
        let Some(head) = self.head()? else {
            return Ok(false);
        };
        let head = parse_oid(&head)
            .ok_or_else(|| HistoryError::Corrupt("HEAD".into(), "not a hex object id".into()))?;
        let tree = self.read_commit(&head)?.tree;
        let mut dirty = false;
        let mut budget = MAX_TREE_ENTRIES;
        self.walk_tree(&tree, "", 0, &mut budget, &mut |path, entry| {
            if dirty || entry.is_tree() || entry.is_gitlink() {
                return;
            }
            let Some(path) = safe_worktree_path(path) else {
                dirty = true;
                return;
            };
            let full = self.repo_root.join(path);
            let bytes = if entry.is_symlink() {
                match std::fs::read_link(&full) {
                    Ok(target) => target.to_string_lossy().into_owned().into_bytes(),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        dirty = true;
                        return;
                    }
                    Err(_) => {
                        dirty = true;
                        return;
                    }
                }
            } else {
                match std::fs::read(&full) {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        dirty = true;
                        return;
                    }
                    Err(_) => {
                        dirty = true;
                        return;
                    }
                }
            };
            let Ok((_, blob)) = self.read_object(&entry.oid) else {
                dirty = true;
                return;
            };
            if bytes.as_slice() != blob.as_slice() && normalize_crlf(&bytes) != blob.as_slice() {
                dirty = true;
            }
        })?;
        if !dirty && !self.untracked_files(1)?.is_empty() {
            // `git status` treats an untracked file as a worktree change. The
            // review limitation must say so, even though the file is not part
            // of any commit.
            dirty = true;
        }
        Ok(dirty)
    }

    /// Worktree files that are not in `HEAD`, bounded and honouring the same
    /// ignore files Git uses.
    ///
    /// This is for review provenance (the fingerprint and the "worktree
    /// differs" note), never for commit evidence: an untracked file is not in
    /// the reviewed object. The walk is bounded because a review must not scan
    /// an unbounded tree.
    pub fn untracked_files(&self, limit: usize) -> Result<Vec<String>, HistoryError> {
        let Some(head) = self.head()? else {
            return Ok(Vec::new());
        };
        let head = parse_oid(&head)
            .ok_or_else(|| HistoryError::Corrupt("HEAD".into(), "not a hex object id".into()))?;
        let tree = self.read_commit(&head)?.tree;
        let mut known = HashSet::new();
        let mut budget = MAX_TREE_ENTRIES;
        self.walk_tree(&tree, "", 0, &mut budget, &mut |path, entry| {
            if !entry.is_tree() {
                known.insert(path.to_string());
            }
        })?;

        let mut untracked = Vec::new();
        let mut visited = 0usize;
        let walker = ignore::WalkBuilder::new(&self.repo_root)
            .hidden(false)
            .follow_links(false)
            .ignore(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();
        for entry in walker {
            if untracked.len() >= limit || visited >= MAX_TREE_ENTRIES {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                continue;
            }
            visited += 1;
            let Ok(relative) = entry.path().strip_prefix(&self.repo_root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative == ".git"
                || relative.starts_with(".git/")
                || relative == ".leveler"
                || relative.starts_with(".leveler/")
            {
                continue;
            }
            if !known.contains(&relative) {
                untracked.push(relative);
            }
        }
        untracked.sort();
        Ok(untracked)
    }

    // ---- revision resolution -------------------------------------------

    fn resolve_commit(&self, rev: &str) -> Result<Oid, HistoryError> {
        let rev = rev.trim();
        if rev.is_empty() {
            return Err(HistoryError::InvalidRevision("empty revision".into()));
        }
        if !rev
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace())
        {
            return Err(HistoryError::InvalidRevision(rev.to_string()));
        }
        if rev.contains("..") || rev.contains("@{") || rev.contains(':') {
            return Err(HistoryError::InvalidRevision(format!(
                "`{rev}` is a range, reflog or path expression, not one revision"
            )));
        }
        let (base, ops) = split_operators(rev)?;
        let mut oid = self.resolve_base(&base)?;
        for op in ops {
            oid = match op {
                Operation::Parent(index) => {
                    let commit = self.peel_commit(oid)?;
                    if index == 0 {
                        commit
                    } else {
                        let parents = self.parents_of(&commit)?;
                        *parents.get(index - 1).ok_or_else(|| {
                            HistoryError::InvalidRevision(format!(
                                "`{rev}` names parent {index}, which does not exist"
                            ))
                        })?
                    }
                }
                Operation::Ancestor(count) => {
                    let mut commit = self.peel_commit(oid)?;
                    for _ in 0..count {
                        commit = *self.parents_of(&commit)?.first().ok_or_else(|| {
                            HistoryError::InvalidRevision(format!(
                                "`{rev}` walks past the root commit"
                            ))
                        })?;
                    }
                    commit
                }
            };
        }
        self.peel_commit(oid)
    }

    fn resolve_base(&self, token: &str) -> Result<Oid, HistoryError> {
        if token.is_empty() {
            return Err(HistoryError::InvalidRevision("empty revision".into()));
        }
        if let Some(oid) = self.resolve_ref_candidates(token)? {
            return Ok(oid);
        }
        if let Some(oid) = parse_oid(token) {
            if self.object_exists(&oid) {
                return Ok(oid);
            }
            return Err(HistoryError::ObjectMissing(token.to_string()));
        }
        if token.len() >= 4 && token.len() < 40 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
            let prefix = token.to_ascii_lowercase();
            let mut matches = self.find_prefix(&prefix)?;
            return match matches.len() {
                0 => Err(HistoryError::InvalidRevision(format!(
                    "unknown revision `{token}` (no local object matches the prefix)"
                ))),
                1 => Ok(matches.remove(0)),
                _ => Err(HistoryError::InvalidRevision(format!(
                    "abbreviated revision `{token}` is ambiguous"
                ))),
            };
        }
        Err(HistoryError::InvalidRevision(format!(
            "unknown revision `{token}`"
        )))
    }

    fn resolve_ref_candidates(&self, token: &str) -> Result<Option<Oid>, HistoryError> {
        if token == "@" {
            return self.read_ref("HEAD").map(Some);
        }
        let mut names = Vec::new();
        if token.starts_with("refs/") || token == "HEAD" {
            names.push(token.to_string());
        } else {
            names.push(token.to_string());
            names.push(format!("refs/{token}"));
            names.push(format!("refs/tags/{token}"));
            names.push(format!("refs/heads/{token}"));
            names.push(format!("refs/remotes/{token}"));
            names.push(format!("refs/remotes/{token}/HEAD"));
        }
        for name in names {
            if !valid_ref_name(&name) {
                continue;
            }
            match self.read_ref(&name) {
                Ok(oid) => return Ok(Some(oid)),
                Err(HistoryError::InvalidRevision(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn read_ref(&self, name: &str) -> Result<Oid, HistoryError> {
        let mut current = name.to_string();
        for _ in 0..8 {
            match self.read_ref_once(&current)? {
                RefValue::Oid(oid) => return Ok(oid),
                RefValue::Symbolic(target) => current = target,
            }
        }
        Err(HistoryError::InvalidRevision(
            "symbolic ref chain is too deep".into(),
        ))
    }

    fn read_ref_once(&self, name: &str) -> Result<RefValue, HistoryError> {
        for root in [&self.git_dir, &self.common_dir] {
            let path = root.join(name);
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(HistoryError::Io(error)),
            };
            let text = text.trim();
            if let Some(target) = text.strip_prefix("ref:") {
                let target = target.trim();
                if !valid_ref_name(target) {
                    return Err(HistoryError::InvalidRevision(format!(
                        "invalid symbolic ref target `{target}`"
                    )));
                }
                return Ok(RefValue::Symbolic(target.to_string()));
            }
            if let Some(oid) = parse_oid(text) {
                return Ok(RefValue::Oid(oid));
            }
            return Err(HistoryError::Corrupt(
                name.to_string(),
                "ref file is neither a symbolic ref nor a hex object id".into(),
            ));
        }
        if let Some(oid) = self.packed.get(name) {
            return Ok(RefValue::Oid(*oid));
        }
        Err(HistoryError::InvalidRevision(format!(
            "unknown ref `{name}`"
        )))
    }

    fn peel_commit(&self, mut oid: Oid) -> Result<Oid, HistoryError> {
        for _ in 0..8 {
            let (kind, data) = self.read_object(&oid)?;
            match kind {
                ObjectKind::Commit => return Ok(oid),
                ObjectKind::Tag => {
                    let text = std::str::from_utf8(&data).map_err(|_| {
                        HistoryError::Corrupt(oid_hex(&oid), "tag header is not UTF-8".into())
                    })?;
                    let target = text
                        .lines()
                        .find_map(|line| line.strip_prefix("object "))
                        .and_then(parse_oid)
                        .ok_or_else(|| {
                            HistoryError::Corrupt(
                                oid_hex(&oid),
                                "tag object has no object header".into(),
                            )
                        })?;
                    oid = target;
                }
                _ => {
                    return Err(HistoryError::InvalidRevision(format!(
                        "object {} is a {}, not a commit",
                        oid_hex(&oid),
                        kind.as_str()
                    )));
                }
            }
        }
        Err(HistoryError::Corrupt(
            oid_hex(&oid),
            "tag chain is too deep".into(),
        ))
    }

    fn parents_of(&self, oid: &Oid) -> Result<Vec<Oid>, HistoryError> {
        if self.shallow.contains(oid) {
            return Ok(Vec::new());
        }
        Ok(self.read_commit(oid)?.parents)
    }

    fn effective_parents(&self, commit: &Commit) -> Vec<Oid> {
        if self.shallow.contains(&commit.oid) {
            Vec::new()
        } else {
            commit.parents.clone()
        }
    }

    fn read_commit(&self, oid: &Oid) -> Result<Commit, HistoryError> {
        let (kind, data) = self.read_object(oid)?;
        if kind != ObjectKind::Commit {
            return Err(HistoryError::Corrupt(
                oid_hex(oid),
                "expected a commit object".into(),
            ));
        }
        let text = std::str::from_utf8(&data).map_err(|_| {
            HistoryError::Corrupt(oid_hex(oid), "commit header is not UTF-8".into())
        })?;
        let header_end = text.find("\n\n").unwrap_or(text.len());
        let header = &text[..header_end];
        let message = if header_end < text.len() {
            text[header_end + 2..].to_string()
        } else {
            String::new()
        };
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        for line in header.lines() {
            if let Some(value) = line.strip_prefix("tree ") {
                tree = parse_oid(value.trim());
            } else if let Some(value) = line.strip_prefix("parent ") {
                if let Some(oid) = parse_oid(value.trim()) {
                    parents.push(oid);
                }
            } else if let Some(value) = line.strip_prefix("author ") {
                author = Some(parse_signature(value));
            } else if let Some(value) = line.strip_prefix("committer ") {
                committer = Some(parse_signature(value));
            }
        }
        Ok(Commit {
            oid: *oid,
            tree: tree.ok_or_else(|| {
                HistoryError::Corrupt(oid_hex(oid), "commit has no tree header".into())
            })?,
            parents,
            author: author.unwrap_or_else(|| parse_signature("")),
            committer: committer.unwrap_or_else(|| parse_signature("")),
            message,
        })
    }

    fn commit_sort_key(&self, oid: &Oid) -> Result<(i64, Oid), HistoryError> {
        let commit = self.read_commit(oid)?;
        Ok((commit.committer.seconds, *oid))
    }

    fn commit_changes_path(&self, commit: &Commit, path: &[u8]) -> Result<bool, HistoryError> {
        let current = self.lookup_path_bytes(&commit.tree, path)?;
        let parents = self.effective_parents(commit);
        if parents.is_empty() {
            return Ok(current.is_some());
        }
        for parent in parents {
            let parent_commit = self.read_commit(&parent)?;
            let parent_entry = self.lookup_path_bytes(&parent_commit.tree, path)?;
            let same = match (&current, &parent_entry) {
                (Some(current), Some(parent)) => {
                    current.oid == parent.oid && current.mode == parent.mode
                }
                (None, None) => true,
                _ => false,
            };
            if same {
                // Git's history simplification omits a commit whose tree for
                // the path equals a parent's tree.
                return Ok(false);
            }
        }
        Ok(true)
    }

    // ---- trees ----------------------------------------------------------

    fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>, HistoryError> {
        let (kind, data) = self.read_object(oid)?;
        if kind != ObjectKind::Tree {
            return Err(HistoryError::Corrupt(
                oid_hex(oid),
                "expected a tree object".into(),
            ));
        }
        let mut entries = Vec::new();
        let mut position = 0;
        while position < data.len() {
            let space = data[position..]
                .iter()
                .position(|byte| *byte == b' ')
                .ok_or_else(|| {
                    HistoryError::Corrupt(oid_hex(oid), "tree entry has no mode separator".into())
                })?
                + position;
            let mode_text = std::str::from_utf8(&data[position..space]).map_err(|_| {
                HistoryError::Corrupt(oid_hex(oid), "tree mode is not ASCII".into())
            })?;
            let mode = u32::from_str_radix(mode_text, 8).map_err(|_| {
                HistoryError::Corrupt(oid_hex(oid), "tree mode is not octal".into())
            })?;
            let name_start = space + 1;
            let nul = data[name_start..]
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| {
                    HistoryError::Corrupt(oid_hex(oid), "tree entry name is not terminated".into())
                })?
                + name_start;
            let name = data[name_start..nul].to_vec();
            if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
                return Err(HistoryError::Corrupt(
                    oid_hex(oid),
                    "tree entry has an invalid name".into(),
                ));
            }
            let oid_start = nul + 1;
            if oid_start + 20 > data.len() {
                return Err(HistoryError::Corrupt(
                    oid_hex(oid),
                    "tree entry is truncated".into(),
                ));
            }
            let mut child = [0u8; 20];
            child.copy_from_slice(&data[oid_start..oid_start + 20]);
            entries.push(TreeEntry {
                mode,
                name,
                oid: child,
            });
            position = oid_start + 20;
            if entries.len() > MAX_TREE_ENTRIES {
                return Err(HistoryError::Limit("tree has too many entries".into()));
            }
        }
        Ok(entries)
    }

    fn lookup_path(&self, tree: &Oid, path: &str) -> Result<Option<TreeEntry>, HistoryError> {
        if path.is_empty() {
            return Ok(None);
        }
        self.lookup_path_bytes(tree, path.as_bytes())
    }

    fn lookup_path_bytes(
        &self,
        tree: &Oid,
        path: &[u8],
    ) -> Result<Option<TreeEntry>, HistoryError> {
        let mut current = *tree;
        let mut parts = path.split(|byte| *byte == b'/').peekable();
        while let Some(part) = parts.next() {
            if part.is_empty() || part == b"." || part == b".." {
                return Err(HistoryError::InvalidPath(
                    String::from_utf8_lossy(path).into_owned(),
                ));
            }
            let entries = self.read_tree(&current)?;
            let Some(entry) = entries.iter().find(|entry| entry.name == part) else {
                return Ok(None);
            };
            if parts.peek().is_some() {
                if !entry.is_tree() {
                    return Ok(None);
                }
                current = entry.oid;
            } else {
                return Ok(Some(entry.clone()));
            }
        }
        Ok(None)
    }

    fn walk_tree(
        &self,
        tree: &Oid,
        prefix: &str,
        depth: usize,
        budget: &mut usize,
        visit: &mut impl FnMut(&str, &TreeEntry),
    ) -> Result<(), HistoryError> {
        if depth > 256 {
            return Err(HistoryError::Limit("tree nesting is too deep".into()));
        }
        let entries = self.read_tree(tree)?;
        for entry in entries {
            if *budget == 0 {
                return Err(HistoryError::Limit(format!(
                    "tree walk exceeded {MAX_TREE_ENTRIES} entries"
                )));
            }
            *budget -= 1;
            let name = String::from_utf8_lossy(&entry.name);
            let path = if prefix.is_empty() {
                name.into_owned()
            } else {
                format!("{prefix}/{name}")
            };
            visit(&path, &entry);
            if entry.is_tree() {
                self.walk_tree(&entry.oid, &path, depth + 1, budget, visit)?;
            }
        }
        Ok(())
    }

    // ---- object store ---------------------------------------------------

    fn read_object(&self, oid: &Oid) -> Result<(ObjectKind, Arc<Vec<u8>>), HistoryError> {
        {
            let cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            if let Some((kind, data)) = cache.entries.get(oid) {
                return Ok((*kind, Arc::clone(data)));
            }
        }
        let (kind, data) = self.read_uncached(oid, 0)?;
        let data = Arc::new(data);
        {
            let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            cache.bytes += data.len();
            cache.entries.insert(*oid, (kind, Arc::clone(&data)));
            if cache.bytes > 64 * 1024 * 1024 {
                cache.entries.clear();
                cache.bytes = data.len();
                cache.entries.insert(*oid, (kind, Arc::clone(&data)));
            }
        }
        Ok((kind, data))
    }

    fn read_uncached(
        &self,
        oid: &Oid,
        depth: usize,
    ) -> Result<(ObjectKind, Vec<u8>), HistoryError> {
        if depth > MAX_DELTA_DEPTH {
            return Err(HistoryError::Limit("delta chain is too deep".into()));
        }
        let hex = oid_hex(oid);
        for dir in &self.object_dirs {
            let loose = dir.root.join(&hex[..2]).join(&hex[2..]);
            if loose.is_file() {
                let (kind, data) = read_loose_object(&loose, oid)?;
                return Ok((kind, data));
            }
            for pack in &dir.packs {
                if let Ok(Some(offset)) = pack.lookup(oid) {
                    return read_pack_object(pack, offset, oid, self, depth);
                }
            }
        }
        Err(HistoryError::ObjectMissing(hex))
    }

    fn object_exists(&self, oid: &Oid) -> bool {
        let hex = oid_hex(oid);
        self.object_dirs.iter().any(|dir| {
            dir.root.join(&hex[..2]).join(&hex[2..]).is_file()
                || dir
                    .packs
                    .iter()
                    .any(|pack| pack.lookup(oid).ok().flatten().is_some())
        })
    }

    fn find_prefix(&self, prefix: &str) -> Result<Vec<Oid>, HistoryError> {
        let mut matches = Vec::new();
        for dir in &self.object_dirs {
            if let Ok(entries) = std::fs::read_dir(dir.root.join(&prefix[..2])) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    let full = format!("{}{}", &prefix[..2], name);
                    if full.starts_with(prefix)
                        && let Some(oid) = parse_oid(&full)
                    {
                        matches.push(oid);
                    }
                }
            }
            for pack in &dir.packs {
                for oid in &pack.oids {
                    let hex = oid_hex(oid);
                    if hex.starts_with(prefix) && !matches.contains(oid) {
                        matches.push(*oid);
                    }
                }
            }
            if matches.len() > 1 {
                return Ok(matches);
            }
        }
        Ok(matches)
    }

    // ---- diffs ----------------------------------------------------------

    fn diff_trees(
        &self,
        from: Option<&Oid>,
        to: Option<&Oid>,
        path: Option<&str>,
    ) -> Result<Vec<FileChange>, HistoryError> {
        let mut changes = Vec::new();
        let filter = path.map(str::to_string);
        let from_entries = match from {
            Some(tree) => self.read_tree(tree)?,
            None => Vec::new(),
        };
        let to_entries = match to {
            Some(tree) => self.read_tree(tree)?,
            None => Vec::new(),
        };
        self.diff_tree_levels(
            &from_entries,
            &to_entries,
            "",
            filter.as_deref(),
            0,
            &mut changes,
        )?;
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(changes)
    }

    fn diff_tree_levels(
        &self,
        from: &[TreeEntry],
        to: &[TreeEntry],
        prefix: &str,
        filter: Option<&str>,
        depth: usize,
        changes: &mut Vec<FileChange>,
    ) -> Result<(), HistoryError> {
        if depth > 256 {
            return Err(HistoryError::Limit("tree nesting is too deep".into()));
        }
        if changes.len() >= MAX_CHANGED_FILES {
            return Err(HistoryError::Limit(format!(
                "diff exceeded {MAX_CHANGED_FILES} changed files"
            )));
        }
        let mut names: Vec<&[u8]> = from.iter().map(|entry| entry.name.as_slice()).collect();
        for entry in to {
            if !names.contains(&entry.name.as_slice()) {
                names.push(entry.name.as_slice());
            }
        }
        names.sort();
        for name in names {
            let old = from.iter().find(|entry| entry.name == name);
            let new = to.iter().find(|entry| entry.name == name);
            let name = String::from_utf8_lossy(name);
            let path = if prefix.is_empty() {
                name.into_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let is_dir = old.is_some_and(TreeEntry::is_tree) || new.is_some_and(TreeEntry::is_tree);
            if !matches_filter(&path, filter, is_dir) {
                continue;
            }
            match (old, new) {
                (Some(old), Some(new)) if old.is_tree() && new.is_tree() => {
                    self.diff_tree_levels(
                        &self.read_tree(&old.oid)?,
                        &self.read_tree(&new.oid)?,
                        &path,
                        filter,
                        depth + 1,
                        changes,
                    )?;
                }
                (Some(old), Some(new)) if old.is_tree() || new.is_tree() => {
                    // A file became a directory or the reverse: report the
                    // deletion and addition sides truthfully.
                    if old.is_tree() {
                        self.collect_tree(&old.oid, &path, Some(old), depth + 1, changes)?;
                    } else {
                        changes.push(FileChange {
                            path: path.clone(),
                            old_mode: Some(old.mode),
                            new_mode: None,
                            old_oid: Some(old.oid),
                            new_oid: None,
                        });
                    }
                    if new.is_tree() {
                        self.collect_tree(&new.oid, &path, None, depth + 1, changes)?;
                    } else {
                        changes.push(FileChange {
                            path,
                            old_mode: None,
                            new_mode: Some(new.mode),
                            old_oid: None,
                            new_oid: Some(new.oid),
                        });
                    }
                }
                (Some(old), Some(new)) if old.oid == new.oid && old.mode == new.mode => {}
                (Some(old), Some(new)) => changes.push(FileChange {
                    path,
                    old_mode: Some(old.mode),
                    new_mode: Some(new.mode),
                    old_oid: Some(old.oid),
                    new_oid: Some(new.oid),
                }),
                (Some(old), None) => {
                    if old.is_tree() {
                        self.collect_tree(&old.oid, &path, Some(old), depth + 1, changes)?;
                    } else {
                        changes.push(FileChange {
                            path,
                            old_mode: Some(old.mode),
                            new_mode: None,
                            old_oid: Some(old.oid),
                            new_oid: None,
                        });
                    }
                }
                (None, Some(new)) => {
                    if new.is_tree() {
                        self.collect_tree(&new.oid, &path, None, depth + 1, changes)?;
                    } else {
                        changes.push(FileChange {
                            path,
                            old_mode: None,
                            new_mode: Some(new.mode),
                            old_oid: None,
                            new_oid: Some(new.oid),
                        });
                    }
                }
                (None, None) => {}
            }
        }
        Ok(())
    }

    /// Flatten a whole tree into file changes. `old` is the tree entry this
    /// side replaces (deletions) or `None` (additions).
    fn collect_tree(
        &self,
        tree: &Oid,
        prefix: &str,
        old: Option<&TreeEntry>,
        depth: usize,
        changes: &mut Vec<FileChange>,
    ) -> Result<(), HistoryError> {
        if depth > 256 {
            return Err(HistoryError::Limit("tree nesting is too deep".into()));
        }
        if changes.len() >= MAX_CHANGED_FILES {
            return Err(HistoryError::Limit(format!(
                "diff exceeded {MAX_CHANGED_FILES} changed files"
            )));
        }
        let entries = self.read_tree(tree)?;
        for entry in entries {
            let name = String::from_utf8_lossy(&entry.name);
            let path = format!("{prefix}/{name}");
            if entry.is_tree() {
                self.collect_tree(&entry.oid, &path, old, depth + 1, changes)?;
            } else if old.is_some() {
                changes.push(FileChange {
                    path,
                    old_mode: Some(entry.mode),
                    new_mode: None,
                    old_oid: Some(entry.oid),
                    new_oid: None,
                });
            } else {
                changes.push(FileChange {
                    path,
                    old_mode: None,
                    new_mode: Some(entry.mode),
                    old_oid: None,
                    new_oid: Some(entry.oid),
                });
            }
        }
        Ok(())
    }

    fn render_stat(&self, changes: &[FileChange]) -> Result<String, HistoryError> {
        if changes.is_empty() {
            return Ok(String::new());
        }
        let mut rows = Vec::new();
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        let mut binary = 0usize;
        for change in changes {
            if change
                .old_mode
                .is_some_and(|mode| mode & 0o170000 == 0o160000)
                || change
                    .new_mode
                    .is_some_and(|mode| mode & 0o170000 == 0o160000)
            {
                rows.push(format!(" {} | 1 +", change.path));
                insertions += 1;
                continue;
            }
            let old = match change.old_oid {
                Some(oid) => self.read_blob(&oid)?,
                None => Vec::new(),
            };
            let new = match change.new_oid {
                Some(oid) => self.read_blob(&oid)?,
                None => Vec::new(),
            };
            if is_binary(&old) || is_binary(&new) {
                binary += 1;
                rows.push(format!(" {} | Bin", change.path));
                continue;
            }
            let (added, removed) = diff_line_counts(&old, &new);
            insertions += added;
            deletions += removed;
            rows.push(format!(
                " {} | {}{}",
                change.path,
                added + removed,
                bar(added, removed)
            ));
        }
        let mut body = rows.join("\n");
        body.push('\n');
        let files = changes.len();
        body.push_str(&format!(
            " {files} file{} changed",
            if files == 1 { "" } else { "s" }
        ));
        if insertions > 0 {
            body.push_str(&format!(
                ", {insertions} insertion{}(+)",
                if insertions == 1 { "" } else { "s" }
            ));
        }
        if deletions > 0 {
            body.push_str(&format!(
                ", {deletions} deletion{}(-)",
                if deletions == 1 { "" } else { "s" }
            ));
        }
        if binary > 0 {
            body.push_str(&format!(
                ", {binary} binary file{}",
                if binary == 1 { "" } else { "s" }
            ));
        }
        body.push('\n');
        Ok(body)
    }

    fn render_changes(&self, changes: &[FileChange]) -> Result<String, HistoryError> {
        let mut body = String::new();
        for change in changes {
            body.push_str(&self.render_one_change(change)?);
            if body.len() > OUTPUT_CAP {
                let mut cut = OUTPUT_CAP;
                while cut > 0 && !body.is_char_boundary(cut) {
                    cut -= 1;
                }
                body.truncate(cut);
                body.push_str(
                    "\n[note] output was truncated; narrow the query (a path) for the rest.\n",
                );
                return Ok(body);
            }
        }
        Ok(body)
    }

    fn render_one_change(&self, change: &FileChange) -> Result<String, HistoryError> {
        let mut body = String::new();
        body.push_str(&format!(
            "diff --git a/{path} b/{path}\n",
            path = change.path
        ));
        let old_mode = change.old_mode;
        let new_mode = change.new_mode;
        if old_mode.is_none() {
            body.push_str(&format!(
                "new file mode {:06o}\n",
                new_mode.unwrap_or(0o100644)
            ));
        } else if new_mode.is_none() {
            body.push_str(&format!(
                "deleted file mode {:06o}\n",
                old_mode.unwrap_or(0o100644)
            ));
        } else if old_mode != new_mode {
            body.push_str(&format!(
                "old mode {:06o}\nnew mode {:06o}\n",
                old_mode.unwrap_or(0),
                new_mode.unwrap_or(0)
            ));
        }
        if old_mode.is_some_and(|mode| mode & 0o170000 == 0o160000)
            || new_mode.is_some_and(|mode| mode & 0o170000 == 0o160000)
        {
            body.push_str(&format!(
                "Subproject commit {}\n",
                change
                    .old_oid
                    .or(change.new_oid)
                    .map(|oid| oid_hex(&oid))
                    .unwrap_or_default()
            ));
            return Ok(body);
        }
        let old = match change.old_oid {
            Some(oid) => self.read_blob(&oid)?,
            None => Vec::new(),
        };
        let new = match change.new_oid {
            Some(oid) => self.read_blob(&oid)?,
            None => Vec::new(),
        };
        if change.old_mode.is_none() || change.new_mode.is_none() {
            // Mode line already states add/delete; the empty side is /dev/null.
        }
        if is_binary(&old) || is_binary(&new) {
            body.push_str(&format!(
                "Binary files a/{} and b/{} differ\n",
                change.path, change.path
            ));
            return Ok(body);
        }
        body.push_str(&format!(
            "--- {}\n+++ {}\n",
            if change.old_oid.is_some() {
                format!("a/{}", change.path)
            } else {
                "/dev/null".to_string()
            },
            if change.new_oid.is_some() {
                format!("b/{}", change.path)
            } else {
                "/dev/null".to_string()
            }
        ));
        body.push_str(&render_hunks(&old, &new));
        Ok(body)
    }

    fn read_blob(&self, oid: &Oid) -> Result<Vec<u8>, HistoryError> {
        let (kind, data) = self.read_object(oid)?;
        if kind != ObjectKind::Blob {
            return Err(HistoryError::Corrupt(
                oid_hex(oid),
                "tree entry does not name a blob".into(),
            ));
        }
        Ok((*data).clone())
    }
}

// ---- refs and repository layout -----------------------------------------

fn discover_repo_root(workspace: &Path) -> Result<PathBuf, HistoryError> {
    let canonical = std::fs::canonicalize(workspace)
        .map_err(|_| HistoryError::NotARepository(workspace.display().to_string()))?;
    for ancestor in canonical.ancestors() {
        if ancestor.join(".git").is_dir() || ancestor.join(".git").is_file() {
            return Ok(ancestor.to_path_buf());
        }
    }
    Err(HistoryError::NotARepository(
        workspace.display().to_string(),
    ))
}

fn check_repository_format(common_dir: &Path) -> Result<(), HistoryError> {
    let text = match std::fs::read_to_string(common_dir.join("config")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(HistoryError::Io(error)),
    };
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().trim_matches('"').to_ascii_lowercase();
        if section == "extensions" && key == "objectformat" && value != "sha1" {
            return Err(HistoryError::Unsupported(format!(
                "object format `{value}` is not supported by the local reader"
            )));
        }
    }
    Ok(())
}

fn read_packed_refs(common_dir: &Path) -> Result<BTreeMap<String, Oid>, HistoryError> {
    let mut refs = BTreeMap::new();
    let text = match std::fs::read_to_string(common_dir.join("packed-refs")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(refs),
        Err(error) => return Err(HistoryError::Io(error)),
    };
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() || line.starts_with('^') {
            continue;
        }
        let Some((oid, name)) = line.split_once(' ') else {
            continue;
        };
        let Some(oid) = parse_oid(oid.trim()) else {
            continue;
        };
        let name = name.trim().to_string();
        if !valid_ref_name(&name) {
            continue;
        }
        refs.insert(name, oid);
    }
    Ok(refs)
}

fn read_shallow(common_dir: &Path) -> Result<HashSet<Oid>, HistoryError> {
    let text = match std::fs::read_to_string(common_dir.join("shallow")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(HistoryError::Io(error)),
    };
    Ok(text
        .lines()
        .filter_map(|line| parse_oid(line.trim()))
        .collect())
}

fn valid_ref_name(name: &str) -> bool {
    if name.is_empty()
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains("..")
        || name.contains("@{")
        || name.contains('\\')
        || name.contains(' ')
        || name.contains('\n')
        || name.contains('\t')
        || name.chars().any(char::is_control)
    {
        return false;
    }
    name.split('/')
        .all(|part| !part.is_empty() && part != "." && part != ".." && !part.ends_with(".lock"))
}

fn collect_object_dirs(root: &Path) -> Result<Vec<ObjectDir>, HistoryError> {
    let mut dirs = Vec::new();
    let mut visited = HashSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !visited.insert(canonical) || dirs.len() > 16 {
            continue;
        }
        let mut packs = Vec::new();
        if let Ok(entries) = std::fs::read_dir(path.join("pack")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) == Some("idx") {
                    packs.push(parse_pack_index(&path)?);
                }
            }
        }
        dirs.push(ObjectDir {
            root: path.clone(),
            packs,
        });
        if let Ok(text) = std::fs::read_to_string(path.join("info/alternates")) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let alternate = Path::new(line);
                pending.push(if alternate.is_absolute() {
                    alternate.to_path_buf()
                } else {
                    path.join(alternate)
                });
            }
        }
    }
    Ok(dirs)
}

// ---- packfiles ----------------------------------------------------------

impl PackIndex {
    fn lookup(&self, oid: &Oid) -> Result<Option<u64>, HistoryError> {
        match self.oids.binary_search(oid) {
            Ok(index) => Ok(Some(self.offsets[index])),
            Err(_) => Ok(None),
        }
    }
}

fn read_u32(bytes: &[u8], position: &mut usize) -> Result<u32, HistoryError> {
    if *position + 4 > bytes.len() {
        return Err(HistoryError::Corrupt(
            "pack index".into(),
            "truncated".into(),
        ));
    }
    let value = u32::from_be_bytes(bytes[*position..*position + 4].try_into().unwrap());
    *position += 4;
    Ok(value)
}

fn parse_pack_index(path: &Path) -> Result<PackIndex, HistoryError> {
    let length = std::fs::metadata(path)?.len();
    if length > 256 * 1024 * 1024 {
        return Err(HistoryError::Limit(format!(
            "pack index {} is too large",
            path.display()
        )));
    }
    let bytes = std::fs::read(path)?;
    let pack_path = path.with_extension("pack");
    let mut position = 0;
    let version = if bytes.starts_with(b"\xfftOc") {
        position = 4;
        let version = read_u32(&bytes, &mut position)?;
        if version != 2 {
            return Err(HistoryError::Unsupported(format!(
                "pack index version {version} in {}",
                path.display()
            )));
        }
        version
    } else {
        1
    };
    let mut fanout = [0u32; 256];
    for slot in &mut fanout {
        *slot = read_u32(&bytes, &mut position)?;
    }
    for index in 1..256 {
        if fanout[index] < fanout[index - 1] {
            return Err(HistoryError::Corrupt(
                path.display().to_string(),
                "pack index fanout is not monotonic".into(),
            ));
        }
    }
    let count = fanout[255] as usize;
    let mut oids = Vec::with_capacity(count);
    let mut offsets = Vec::with_capacity(count);
    if version == 1 {
        for _ in 0..count {
            let offset = read_u32(&bytes, &mut position)? as u64;
            if position + 20 > bytes.len() {
                return Err(HistoryError::Corrupt(
                    path.display().to_string(),
                    "pack index v1 is truncated".into(),
                ));
            }
            let mut oid = [0u8; 20];
            oid.copy_from_slice(&bytes[position..position + 20]);
            position += 20;
            oids.push(oid);
            offsets.push(offset);
        }
    } else {
        if position + count * 20 > bytes.len() {
            return Err(HistoryError::Corrupt(
                path.display().to_string(),
                "pack index object names are truncated".into(),
            ));
        }
        for _ in 0..count {
            let mut oid = [0u8; 20];
            oid.copy_from_slice(&bytes[position..position + 20]);
            position += 20;
            oids.push(oid);
        }
        position += count * 4; // CRC table
        let offset_table_start = position;
        let mut large = Vec::new();
        if position + count * 4 <= bytes.len() {
            position += count * 4;
        }
        while position + 8 <= bytes.len().saturating_sub(40) {
            large.push(u64::from_be_bytes(
                bytes[position..position + 8].try_into().unwrap(),
            ));
            position += 8;
        }
        for index in 0..count {
            let raw = u32::from_be_bytes(
                bytes[offset_table_start + index * 4..offset_table_start + index * 4 + 4]
                    .try_into()
                    .unwrap(),
            );
            if raw & 0x8000_0000 != 0 {
                let slot = (raw & 0x7fff_ffff) as usize;
                offsets.push(*large.get(slot).ok_or_else(|| {
                    HistoryError::Corrupt(
                        path.display().to_string(),
                        "pack index large-offset slot is missing".into(),
                    )
                })?);
            } else {
                offsets.push(raw as u64);
            }
        }
    }
    if oids.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(HistoryError::Corrupt(
            path.display().to_string(),
            "pack index object names are not sorted".into(),
        ));
    }
    Ok(PackIndex {
        oids,
        offsets,
        pack_path,
    })
}

fn read_pack_object(
    pack: &PackIndex,
    offset: u64,
    expected: &Oid,
    history: &RepositoryHistory,
    depth: usize,
) -> Result<(ObjectKind, Vec<u8>), HistoryError> {
    let mut file = std::fs::File::open(&pack.pack_path)?;
    file.seek(SeekFrom::Start(offset))?;
    let (kind, data) = read_pack_entry(&mut file, offset, pack, history, depth)?;
    verify_object(expected, kind, &data)?;
    Ok((kind, data))
}

fn read_pack_entry(
    file: &mut std::fs::File,
    offset: u64,
    pack: &PackIndex,
    history: &RepositoryHistory,
    depth: usize,
) -> Result<(ObjectKind, Vec<u8>), HistoryError> {
    if depth > MAX_DELTA_DEPTH {
        return Err(HistoryError::Limit("delta chain is too deep".into()));
    }
    let mut first = [0u8; 1];
    file.read_exact(&mut first)?;
    let kind_bits = (first[0] >> 4) & 0x7;
    let mut size = (first[0] & 0x0f) as u64;
    let mut shift = 4;
    let mut byte = first[0];
    while byte & 0x80 != 0 {
        file.read_exact(&mut first)?;
        byte = first[0];
        size |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
        if shift > 63 {
            return Err(HistoryError::Corrupt(
                "pack".into(),
                "pack object size is too large".into(),
            ));
        }
    }
    match kind_bits {
        1..=4 => {
            if size as usize > MAX_OBJECT_BYTES {
                return Err(HistoryError::Limit(format!(
                    "packed object is {size} bytes"
                )));
            }
            let mut data = Vec::with_capacity(size as usize);
            let decoder = flate2::read::ZlibDecoder::new(&mut *file);
            decoder.take(size + 1).read_to_end(&mut data)?;
            if data.len() as u64 != size {
                return Err(HistoryError::Corrupt(
                    "pack".into(),
                    "object size does not match its header".into(),
                ));
            }
            let kind = match kind_bits {
                1 => ObjectKind::Commit,
                2 => ObjectKind::Tree,
                3 => ObjectKind::Blob,
                _ => ObjectKind::Tag,
            };
            Ok((kind, data))
        }
        6 => {
            // OFS_DELTA: the base sits backwards from this object's offset.
            let mut byte = [0u8; 1];
            file.read_exact(&mut byte)?;
            let mut distance = (byte[0] & 0x7f) as u64;
            while byte[0] & 0x80 != 0 {
                file.read_exact(&mut byte)?;
                distance = ((distance + 1) << 7) | (byte[0] & 0x7f) as u64;
            }
            let base_offset = offset.checked_sub(distance).ok_or_else(|| {
                HistoryError::Corrupt("pack".into(), "delta base offset is invalid".into())
            })?;
            let mut base_file = std::fs::File::open(&pack.pack_path)?;
            base_file.seek(SeekFrom::Start(base_offset))?;
            let (base_kind, base) =
                read_pack_entry(&mut base_file, base_offset, pack, history, depth + 1)?;
            let delta = read_delta_payload(file, size)?;
            let data = apply_delta(&base, &delta)?;
            Ok((base_kind, data))
        }
        7 => {
            let mut base_oid = [0u8; 20];
            file.read_exact(&mut base_oid)?;
            let (base_kind, base) = history
                .read_object(&base_oid)
                .map(|(kind, data)| (kind, (*data).clone()))?;
            let delta = read_delta_payload(file, size)?;
            let data = apply_delta(&base, &delta)?;
            Ok((base_kind, data))
        }
        other => Err(HistoryError::Corrupt(
            "pack".into(),
            format!("unknown object type {other}"),
        )),
    }
}

fn read_delta_payload(file: &mut std::fs::File, size: u64) -> Result<Vec<u8>, HistoryError> {
    if size as usize > MAX_OBJECT_BYTES {
        return Err(HistoryError::Limit(format!(
            "delta payload is {size} bytes"
        )));
    }
    let mut delta = Vec::with_capacity(size as usize);
    let decoder = flate2::read::ZlibDecoder::new(file);
    decoder.take(size + 1).read_to_end(&mut delta)?;
    if delta.len() as u64 != size {
        return Err(HistoryError::Corrupt(
            "pack".into(),
            "delta size does not match its header".into(),
        ));
    }
    Ok(delta)
}

fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, HistoryError> {
    let mut position = 0;
    let source_size = read_delta_varint(delta, &mut position)?;
    let target_size = read_delta_varint(delta, &mut position)?;
    if source_size as usize != base.len() || target_size as usize > MAX_OBJECT_BYTES {
        return Err(HistoryError::Corrupt(
            "pack".into(),
            "delta source or target size is invalid".into(),
        ));
    }
    let mut output = Vec::with_capacity(target_size as usize);
    while position < delta.len() {
        let command = delta[position];
        position += 1;
        if command & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for shift in 0..4 {
                if command & (1 << shift) != 0 {
                    let byte = *delta.get(position).ok_or_else(|| {
                        HistoryError::Corrupt(
                            "pack".into(),
                            "delta copy offset is truncated".into(),
                        )
                    })?;
                    position += 1;
                    offset |= (byte as usize) << (shift * 8);
                }
            }
            for shift in 0..3 {
                if command & (1 << (4 + shift)) != 0 {
                    let byte = *delta.get(position).ok_or_else(|| {
                        HistoryError::Corrupt(
                            "pack".into(),
                            "delta copy length is truncated".into(),
                        )
                    })?;
                    position += 1;
                    size |= (byte as usize) << (shift * 8);
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            let end = offset.checked_add(size).ok_or_else(|| {
                HistoryError::Corrupt("pack".into(), "delta copy range overflows".into())
            })?;
            if end > base.len() || output.len() + size > target_size as usize {
                return Err(HistoryError::Corrupt(
                    "pack".into(),
                    "delta copy is outside its source or target".into(),
                ));
            }
            output.extend_from_slice(&base[offset..end]);
        } else if command != 0 {
            let size = command as usize;
            let end = position.checked_add(size).ok_or_else(|| {
                HistoryError::Corrupt("pack".into(), "delta insert range overflows".into())
            })?;
            if end > delta.len() || output.len() + size > target_size as usize {
                return Err(HistoryError::Corrupt(
                    "pack".into(),
                    "delta insert is outside its payload or target".into(),
                ));
            }
            output.extend_from_slice(&delta[position..end]);
            position = end;
        } else {
            return Err(HistoryError::Corrupt(
                "pack".into(),
                "delta opcode 0 is reserved".into(),
            ));
        }
    }
    if output.len() != target_size as usize {
        return Err(HistoryError::Corrupt(
            "pack".into(),
            "delta result size does not match its header".into(),
        ));
    }
    Ok(output)
}

fn read_delta_varint(bytes: &[u8], position: &mut usize) -> Result<u64, HistoryError> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*position).ok_or_else(|| {
            HistoryError::Corrupt("pack".into(), "delta varint is truncated".into())
        })?;
        *position += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 63 {
            return Err(HistoryError::Corrupt(
                "pack".into(),
                "delta varint is too large".into(),
            ));
        }
    }
    Ok(value)
}

// ---- loose objects ------------------------------------------------------

fn read_loose_object(path: &Path, oid: &Oid) -> Result<(ObjectKind, Vec<u8>), HistoryError> {
    // Cap the compressed read too: a hostile repository can name a huge file
    // under objects/ and the decompressor cap only applies after it is read.
    if std::fs::metadata(path)?.len() > MAX_OBJECT_BYTES as u64 {
        return Err(HistoryError::Limit(format!(
            "compressed loose object {} exceeds {MAX_OBJECT_BYTES} bytes",
            oid_hex(oid)
        )));
    }
    let compressed = std::fs::read(path)?;
    let decoder = flate2::read::ZlibDecoder::new(compressed.as_slice());
    let mut raw = Vec::new();
    decoder
        .take(MAX_OBJECT_BYTES as u64 + 1)
        .read_to_end(&mut raw)?;
    if raw.len() > MAX_OBJECT_BYTES {
        return Err(HistoryError::Limit(format!(
            "loose object {} exceeds {MAX_OBJECT_BYTES} bytes",
            oid_hex(oid)
        )));
    }
    let nul = raw.iter().position(|byte| *byte == 0).ok_or_else(|| {
        HistoryError::Corrupt(oid_hex(oid), "loose header is unterminated".into())
    })?;
    let header = std::str::from_utf8(&raw[..nul])
        .map_err(|_| HistoryError::Corrupt(oid_hex(oid), "loose header is not ASCII".into()))?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| HistoryError::Corrupt(oid_hex(oid), "loose header has no size".into()))?;
    let kind = match kind {
        "commit" => ObjectKind::Commit,
        "tree" => ObjectKind::Tree,
        "blob" => ObjectKind::Blob,
        "tag" => ObjectKind::Tag,
        other => {
            return Err(HistoryError::Corrupt(
                oid_hex(oid),
                format!("unknown loose object type `{other}`"),
            ));
        }
    };
    let size: usize = size
        .parse()
        .map_err(|_| HistoryError::Corrupt(oid_hex(oid), "loose size is not a number".into()))?;
    let data = raw[nul + 1..].to_vec();
    if data.len() != size {
        return Err(HistoryError::Corrupt(
            oid_hex(oid),
            "loose object size does not match its header".into(),
        ));
    }
    verify_object(oid, kind, &data)?;
    Ok((kind, data))
}

fn verify_object(expected: &Oid, kind: ObjectKind, data: &[u8]) -> Result<(), HistoryError> {
    let mut hasher = Sha1::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update(b" ");
    hasher.update(data.len().to_string().as_bytes());
    hasher.update([0]);
    hasher.update(data);
    let actual: Oid = hasher.finalize().into();
    if &actual != expected {
        return Err(HistoryError::Corrupt(
            oid_hex(expected),
            "object content does not hash to its id".into(),
        ));
    }
    Ok(())
}

// ---- revisions ----------------------------------------------------------

enum Operation {
    Parent(usize),
    Ancestor(usize),
}

fn split_operators(rev: &str) -> Result<(String, Vec<Operation>), HistoryError> {
    let mut rev = rev.to_string();
    // The callers spell commit-peeling explicitly; it is an identity operation
    // once the base is a commit.
    while let Some(stripped) = rev.strip_suffix("^{commit}") {
        rev = stripped.to_string();
    }
    while let Some(stripped) = rev.strip_suffix("^{}") {
        rev = stripped.to_string();
    }
    let mut split = rev.len();
    for (index, character) in rev.char_indices() {
        if character == '~' || character == '^' {
            split = index;
            break;
        }
    }
    let base = rev[..split].to_string();
    let mut operations = Vec::new();
    let mut rest = &rev[split..];
    while !rest.is_empty() {
        let mut characters = rest.chars();
        let operator = characters.next().unwrap();
        let after = characters.as_str();
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let count = if digits.is_empty() {
            1
        } else {
            digits
                .parse::<usize>()
                .map_err(|_| HistoryError::InvalidRevision(rev.clone()))?
        };
        rest = &after[digits.len()..];
        operations.push(match operator {
            '~' => Operation::Ancestor(count),
            '^' => Operation::Parent(count),
            _ => {
                return Err(HistoryError::InvalidRevision(rev.clone()));
            }
        });
    }
    if base.is_empty() {
        return Err(HistoryError::InvalidRevision(rev));
    }
    Ok((base, operations))
}

// ---- rendering helpers --------------------------------------------------

fn commit_view(commit: &Commit) -> CommitView {
    CommitView {
        oid: oid_hex(&commit.oid),
        parents: commit.parents.iter().map(oid_hex).collect(),
        author_name: commit.author.name.clone(),
        author_email: commit.author.email.clone(),
        author_time: commit.author.iso8601(),
        committer_time: commit.committer.iso8601(),
        message: commit.message.clone(),
    }
}

fn render_commit_header(commit: &Commit) -> String {
    let mut body = format!("commit {}\n", oid_hex(&commit.oid));
    body.push_str(&format!(
        "Author: {} <{}>\n",
        commit.author.name, commit.author.email
    ));
    body.push_str(&format!("Date:   {}\n\n", commit.author.human()));
    for line in commit.message.lines() {
        if line.is_empty() {
            body.push('\n');
        } else {
            body.push_str(&format!("    {line}\n"));
        }
    }
    body
}

fn cap_output(mut text: String) -> String {
    if text.len() <= OUTPUT_CAP {
        return text;
    }
    let mut cut = OUTPUT_CAP;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str("\n[note] output was truncated; narrow the query for the rest.\n");
    text
}

fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|byte| *byte == 0)
}

fn normalize_crlf(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            normalized.push(b'\n');
            index += 2;
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
    }
    normalized
}

fn safe_worktree_path(path: &str) -> Option<&Path> {
    let path = Path::new(path);
    if path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Some(path)
    } else {
        None
    }
}

fn path_matches(path: &str, base: Option<&str>) -> bool {
    let Some(base) = base else {
        return true;
    };
    path == base
        || path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn matches_filter(path: &str, filter: Option<&str>, is_dir: bool) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    if path == filter {
        return true;
    }
    path.strip_prefix(filter)
        .is_some_and(|rest| rest.starts_with('/'))
        || (is_dir && filter.starts_with(&format!("{path}/")))
}

fn basic_regex(pattern: &str) -> Result<regex::bytes::Regex, HistoryError> {
    let mut translated = String::with_capacity(pattern.len());
    let mut characters = pattern.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next() {
                Some(escaped @ ('(' | ')' | '|' | '+' | '?' | '{' | '}')) => {
                    translated.push(escaped);
                }
                Some(escaped) => {
                    translated.push('\\');
                    translated.push(escaped);
                }
                None => {
                    return Err(HistoryError::InvalidRevision(
                        "search pattern ends with a backslash".into(),
                    ));
                }
            },
            '(' | ')' | '|' | '+' | '?' | '{' | '}' => {
                translated.push('\\');
                translated.push(character);
            }
            other => translated.push(other),
        }
    }
    regex::bytes::Regex::new(&translated)
        .map_err(|error| HistoryError::InvalidRevision(format!("invalid search pattern: {error}")))
}

// ---- line diff ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffOp {
    Equal { len: usize },
    Delete { len: usize },
    Insert { len: usize },
}

fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&[u8]> = data.split_inclusive(|byte| *byte == b'\n').collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

fn diff_ops(old: &[u8], new: &[u8]) -> Vec<DiffOp> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_middle = &old_lines[prefix..old_lines.len() - suffix];
    let new_middle = &new_lines[prefix..new_lines.len() - suffix];
    let mut ops = Vec::new();
    if prefix > 0 {
        ops.push(DiffOp::Equal { len: prefix });
    }
    ops.extend(lcs_ops(old_middle, new_middle));
    if suffix > 0 {
        ops.push(DiffOp::Equal { len: suffix });
    }
    ops
}

fn lcs_ops(old: &[&[u8]], new: &[&[u8]]) -> Vec<DiffOp> {
    if old.is_empty() {
        return (!new.is_empty())
            .then_some(DiffOp::Insert { len: new.len() })
            .into_iter()
            .collect();
    }
    if new.is_empty() {
        return vec![DiffOp::Delete { len: old.len() }];
    }
    if old.len().saturating_mul(new.len()) > MAX_DIFF_CELLS {
        return vec![
            DiffOp::Delete { len: old.len() },
            DiffOp::Insert { len: new.len() },
        ];
    }
    let width = new.len() + 1;
    let mut table = vec![0u32; (old.len() + 1) * width];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            table[i * width + j] = if old[i] == new[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            push_op(&mut ops, DiffOp::Equal { len: 1 });
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            push_op(&mut ops, DiffOp::Delete { len: 1 });
            i += 1;
        } else {
            push_op(&mut ops, DiffOp::Insert { len: 1 });
            j += 1;
        }
    }
    if i < old.len() {
        push_op(&mut ops, DiffOp::Delete { len: old.len() - i });
    }
    if j < new.len() {
        push_op(&mut ops, DiffOp::Insert { len: new.len() - j });
    }
    ops
}

fn push_op(ops: &mut Vec<DiffOp>, op: DiffOp) {
    if let Some(last) = ops.last_mut() {
        match (&mut *last, op) {
            (DiffOp::Equal { len }, DiffOp::Equal { len: add })
            | (DiffOp::Delete { len }, DiffOp::Delete { len: add })
            | (DiffOp::Insert { len }, DiffOp::Insert { len: add }) => {
                *len += add;
                return;
            }
            _ => {}
        }
    }
    ops.push(op);
}

fn equal_line_mapping(ops: &[DiffOp]) -> HashMap<usize, usize> {
    let mut mapping = HashMap::new();
    let mut old_line = 0;
    let mut new_line = 0;
    for op in ops {
        match op {
            DiffOp::Equal { len } => {
                for offset in 0..*len {
                    mapping.insert(new_line + offset, old_line + offset);
                }
                old_line += len;
                new_line += len;
            }
            DiffOp::Delete { len } => old_line += len,
            DiffOp::Insert { len } => new_line += len,
        }
    }
    mapping
}

fn render_hunks(old: &[u8], new: &[u8]) -> String {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let ops = diff_ops(old, new);
    #[derive(Clone, Copy)]
    enum Line {
        Context(usize, usize),
        Delete(usize, usize),
        Insert(usize, usize),
    }
    let mut lines = Vec::new();
    let (mut old_index, mut new_index) = (0, 0);
    for op in &ops {
        match op {
            DiffOp::Equal { len } => {
                for _ in 0..*len {
                    lines.push(Line::Context(old_index, new_index));
                    old_index += 1;
                    new_index += 1;
                }
            }
            DiffOp::Delete { len } => {
                for _ in 0..*len {
                    lines.push(Line::Delete(old_index, new_index));
                    old_index += 1;
                }
            }
            DiffOp::Insert { len } => {
                for _ in 0..*len {
                    lines.push(Line::Insert(old_index, new_index));
                    new_index += 1;
                }
            }
        }
    }
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !matches!(line, Line::Context(_, _)))
        .map(|(index, _)| index)
        .collect();
    if changed.is_empty() {
        return String::new();
    }
    const CONTEXT: usize = 3;
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut start = changed[0].saturating_sub(CONTEXT);
    let mut end = (changed[0] + CONTEXT + 1).min(lines.len());
    for index in &changed[1..] {
        let candidate_start = index.saturating_sub(CONTEXT);
        if candidate_start <= end {
            end = (*index + CONTEXT + 1).min(lines.len());
        } else {
            hunks.push((start, end));
            start = candidate_start;
            end = (*index + CONTEXT + 1).min(lines.len());
        }
    }
    hunks.push((start, end));
    let mut body = String::new();
    for (start, end) in hunks {
        let mut old_start = None;
        let mut new_start = None;
        let mut old_count = 0;
        let mut new_count = 0;
        for line in &lines[start..end] {
            match line {
                Line::Context(old_line, new_line) => {
                    old_start.get_or_insert(*old_line);
                    new_start.get_or_insert(*new_line);
                    old_count += 1;
                    new_count += 1;
                }
                Line::Delete(old_line, new_line) => {
                    old_start.get_or_insert(*old_line);
                    new_start.get_or_insert(*new_line);
                    old_count += 1;
                }
                Line::Insert(old_line, new_line) => {
                    old_start.get_or_insert(*old_line);
                    new_start.get_or_insert(*new_line);
                    new_count += 1;
                }
            }
        }
        let old_start = old_start.unwrap_or(0);
        let new_start = new_start.unwrap_or(0);
        body.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            if old_count == 0 { 0 } else { old_start + 1 },
            old_count,
            if new_count == 0 { 0 } else { new_start + 1 },
            new_count
        ));
        for line in &lines[start..end] {
            match line {
                Line::Context(old_line, _) => {
                    push_diff_line(&mut body, b' ', old_lines[*old_line]);
                }
                Line::Delete(old_line, _) => {
                    push_diff_line(&mut body, b'-', old_lines[*old_line]);
                }
                Line::Insert(_, new_line) => {
                    push_diff_line(&mut body, b'+', new_lines[*new_line]);
                }
            }
        }
    }
    body
}

fn push_diff_line(body: &mut String, prefix: u8, line: &[u8]) {
    body.push(prefix as char);
    match line.strip_suffix(b"\n") {
        Some(content) => {
            body.push_str(&String::from_utf8_lossy(content));
            body.push('\n');
        }
        None => {
            body.push_str(&String::from_utf8_lossy(line));
            body.push_str("\n\\ No newline at end of file\n");
        }
    }
}

#[derive(Debug, Clone)]
struct FileChange {
    path: String,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    old_oid: Option<Oid>,
    new_oid: Option<Oid>,
}

fn diff_line_counts(old: &[u8], new: &[u8]) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for op in diff_ops(old, new) {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { len } => removed += len,
            DiffOp::Insert { len } => added += len,
        }
    }
    (added, removed)
}

fn bar(added: usize, removed: usize) -> String {
    let total = added + removed;
    if total == 0 {
        return String::new();
    }
    let width = total.min(40);
    let plus = if total == 0 {
        0
    } else {
        (added * width).div_ceil(total)
    };
    let minus = width.saturating_sub(plus);
    format!(" {}{}", "+".repeat(plus), "-".repeat(minus))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct Fixture {
        dir: tempfile::TempDir,
        first: String,
        second: String,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "t@t"]);
        git(path, &["config", "user.name", "t"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::write(path.join("src/a.rs"), "pub fn bug_marker() {}\n").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-q", "-m", "A: introduce"]);
        let first = git(path, &["rev-parse", "HEAD"]);
        std::fs::write(path.join("src/a.rs"), "pub fn fixed_marker() {}\n").unwrap();
        git(path, &["commit", "-q", "-am", "B: fix"]);
        let second = git(path, &["rev-parse", "HEAD"]);
        std::fs::write(path.join("src/a.rs"), "pub fn dirty_worktree() {}\n").unwrap();
        Fixture { dir, first, second }
    }

    #[test]
    fn a_file_comes_from_the_requested_commit_not_the_worktree() {
        let fixture = fixture();
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        let first = history.file(&fixture.first, "src/a.rs").unwrap();
        assert!(first.contains("bug_marker"), "{first}");
        assert!(!first.contains("dirty_worktree"));
        let second = history.file(&fixture.second, "src/a.rs").unwrap();
        assert!(second.contains("fixed_marker"), "{second}");
        assert_eq!(history.resolve_commit_id("HEAD").unwrap(), fixture.second);
    }

    #[test]
    fn search_list_and_log_follow_the_requested_revision() {
        let fixture = fixture();
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        let at_a = history.search(&fixture.first, "bug_marker", None).unwrap();
        assert!(at_a.contains("bug_marker"), "{at_a}");
        let at_b = history.search(&fixture.second, "bug_marker", None).unwrap();
        assert!(!at_b.contains("bug_marker"), "{at_b}");
        let list = history.list(&fixture.first, Some("src")).unwrap();
        assert_eq!(list, "src/a.rs");
        let log = history.log(&fixture.second, 1, None).unwrap();
        assert!(log.contains("B: fix"), "{log}");
        assert!(log.starts_with(&fixture.second), "{log}");
    }

    #[test]
    fn blame_attributes_introduced_and_changed_lines() {
        let fixture = fixture();
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        let blame = history
            .blame(&fixture.second, "src/a.rs", None, None)
            .unwrap();
        assert!(blame.contains("fixed_marker"), "{blame}");
        assert!(blame.contains(&fixture.second[..12]), "{blame}");
    }

    #[test]
    fn show_renders_the_commit_patch_and_a_merge_uses_its_first_parent() {
        let fixture = fixture();
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        let show = history.show(&fixture.second, None).unwrap();
        assert!(show.contains("B: fix"), "{show}");
        assert!(show.contains("+pub fn fixed_marker"), "{show}");

        let path = fixture.dir.path();
        git(path, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(path.join("src/b.rs"), "pub fn feature_marker() {}\n").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-q", "-m", "feature: add b"]);
        git(path, &["checkout", "-q", "main"]);
        std::fs::write(path.join("src/c.rs"), "pub fn main_marker() {}\n").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-q", "-m", "main: add c"]);
        git(
            path,
            &["merge", "-q", "--no-ff", "feature", "-m", "merge feature"],
        );
        let merge = git(path, &["rev-parse", "HEAD"]);
        let show = history.show(&merge, None).unwrap();
        assert!(show.contains("merge commit"), "{show}");
        assert!(show.contains("feature_marker"), "{show}");
    }

    #[test]
    fn ancestry_and_unknown_revisions_are_explicit() {
        let fixture = fixture();
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        assert!(
            history
                .is_ancestor(&fixture.first, &fixture.second)
                .unwrap()
        );
        assert!(
            !history
                .is_ancestor(&fixture.second, &fixture.first)
                .unwrap()
        );
        assert!(matches!(
            history.resolve_commit_id("no-such-rev"),
            Err(HistoryError::InvalidRevision(_))
        ));
        assert!(matches!(
            history.resolve_commit_id("HEAD~9"),
            Err(HistoryError::InvalidRevision(_))
        ));
    }

    #[test]
    fn packed_objects_are_read_without_a_git_process() {
        let fixture = fixture();
        // Rewrite every object into a pack and drop the loose copies.
        git(fixture.dir.path(), &["gc", "--aggressive", "--prune=now"]);
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        let first = history.file(&fixture.first, "src/a.rs").unwrap();
        assert!(first.contains("bug_marker"), "{first}");
        let log = history.log(&fixture.second, 2, None).unwrap();
        assert!(log.contains("A: introduce"), "{log}");
        assert!(log.contains("B: fix"), "{log}");
        let show = history.show(&fixture.first, None).unwrap();
        assert!(show.contains("+pub fn bug_marker"), "{show}");
    }

    /// Windows (and Git) may mark a loose object read-only. Clearing that bit
    /// is the only way to simulate corruption in a fixture; the lint's Unix
    /// warning does not apply to this `#[cfg(windows)]` call.
    #[allow(clippy::permissions_set_readonly_false)]
    #[test]
    fn a_tampered_object_is_refused_instead_of_rendered() {
        let fixture = fixture();
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        let oid = history.resolve_commit_id(&fixture.first).unwrap();
        let loose = fixture
            .dir
            .path()
            .join(".git/objects")
            .join(&oid[..2])
            .join(&oid[2..]);
        assert!(loose.is_file(), "fixture should start with a loose object");
        let mut permissions = std::fs::metadata(&loose).unwrap().permissions();
        #[cfg(windows)]
        permissions.set_readonly(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(permissions.mode() | 0o200);
        }
        std::fs::set_permissions(&loose, permissions).unwrap();
        std::fs::write(&loose, b"not zlib").unwrap();
        // A new reader has no cached copy of the object.
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        let error = history.file(&fixture.first, "src/a.rs").unwrap_err();
        assert!(
            matches!(error, HistoryError::Io(_) | HistoryError::Corrupt(_, _)),
            "{error}"
        );
    }

    #[test]
    fn the_worktree_dirty_check_reads_the_tree_and_worktree() {
        let fixture = fixture();
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        assert!(history.worktree_dirty().unwrap());
        git(fixture.dir.path(), &["checkout", "--", "src/a.rs"]);
        assert!(!history.worktree_dirty().unwrap());
        assert!(
            history.untracked_files(10).unwrap().is_empty(),
            "the repository's own metadata must not count as an untracked file"
        );
    }

    #[test]
    fn a_replaced_git_directory_is_not_mistaken_for_the_old_repository() {
        let fixture = fixture();
        let old = fixture.dir.path().join(".git");
        std::fs::rename(&old, fixture.dir.path().join("old-git")).unwrap();
        git(fixture.dir.path(), &["init", "-q", "-b", "main"]);
        let history = RepositoryHistory::open(fixture.dir.path()).unwrap();
        assert!(history.head().unwrap().is_none());
    }
}
