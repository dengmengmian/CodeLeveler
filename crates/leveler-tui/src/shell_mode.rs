//! Local shell mode: input-mode state, the completion tokenizer, and the
//! deterministic file/directory/$PATH providers.
//!
//! The tokenizer is deliberately a small POSIX subset, matching what the
//! product needs to find and replace the token under the cursor. It
//! understands quotes, backslash escapes, whitespace, `|`/`;`/`&` segment
//! separators and `<`/`>` redirect targets. It does not model subshells,
//! here-docs, glob expansion or `~user`; anything it cannot parse safely
//! degrades to "no completion" rather than rewriting the line.
//!
//! Completion is deterministic: no model, no network, no writes. Scanning runs
//! off the UI thread (see `Effect::ShellComplete`) and every result is bound to
//! the text, cursor and cwd it was produced for.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Which surface the composer is currently typing for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputMode {
    /// Ordinary chat / goal input.
    #[default]
    Normal,
    /// Local command (`!`). The `!` is a mode marker, never part of the text.
    Shell,
}

/// Quote state at the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteStyle {
    None,
    Double,
    Single,
}

/// The shell token under the cursor plus the segment facts completion needs.
#[derive(Debug, Clone)]
pub struct CurrentToken {
    /// Byte offset where the token starts (an opening quote is part of it).
    pub start: usize,
    /// Unquoted/unescaped value typed so far.
    pub value: String,
    /// `value` length right after its last `/` (`None`: no slash).
    pub dir_value_len: Option<usize>,
    /// Byte offset just after the last raw `/` (equals `start` when none).
    pub dir_raw_end: usize,
    /// Quote state at the cursor.
    pub quote: QuoteStyle,
    /// Byte offset of the still-open quote (when `quote != None`).
    pub open_quote_idx: usize,
    /// Quote structure of the replaced component when no quote is open at the
    /// cursor: `(style, opener_inside_component)`.
    pub closed_quote: Option<(QuoteStyle, bool)>,
    /// Byte-aligned with `value`: true where the char was typed unquoted and
    /// unescaped, the only spelling the shell expands `~`/`$` in.
    pub plain_mask: Vec<bool>,
    /// Completed tokens before this one in the current segment.
    pub tokens_before: usize,
    /// The segment's command word (first non-redirect-target token value).
    pub command: Option<String>,
    /// Token directly follows `<`/`>`.
    pub after_redirect: bool,
}

#[derive(Debug, Default)]
struct TokenBuild {
    start: usize,
    value: String,
    dir_value_len: Option<usize>,
    dir_raw_end: usize,
    after_redirect: bool,
    last_close: Option<(usize, usize, QuoteStyle)>,
    plain_mask: Vec<bool>,
}

impl TokenBuild {
    fn push(&mut self, i: usize, c: char, plain: bool) {
        self.value.push(c);
        self.plain_mask
            .extend(std::iter::repeat_n(plain, c.len_utf8()));
        if c == '/' {
            self.dir_value_len = Some(self.value.len());
            self.dir_raw_end = i + 1;
        }
    }
}

fn ensure_token<'a>(
    cur: &'a mut Option<TokenBuild>,
    i: usize,
    pending_redirect: &mut bool,
) -> &'a mut TokenBuild {
    cur.get_or_insert_with(|| TokenBuild {
        start: i,
        dir_raw_end: i,
        after_redirect: std::mem::take(pending_redirect),
        ..TokenBuild::default()
    })
}

fn finish_token(
    cur: &mut Option<TokenBuild>,
    tokens_before: &mut usize,
    command: &mut Option<String>,
) {
    if let Some(t) = cur.take() {
        *tokens_before += 1;
        if command.is_none() && !t.after_redirect {
            *command = Some(t.value);
        }
    }
}

/// Scan the cursor prefix and return the token being typed.
pub fn parse_current_token(prefix: &str) -> CurrentToken {
    let mut cur: Option<TokenBuild> = None;
    let mut quote = QuoteStyle::None;
    let mut open_quote_idx = 0usize;
    let mut escape = false;
    let mut tokens_before = 0usize;
    let mut command: Option<String> = None;
    let mut pending_redirect = false;

    for (i, c) in prefix.char_indices() {
        if escape {
            escape = false;
            let t = ensure_token(&mut cur, i, &mut pending_redirect);
            match quote {
                QuoteStyle::None => t.push(i, c, false),
                QuoteStyle::Double => {
                    if !matches!(c, '"' | '\\' | '$' | '`') {
                        t.push(i, '\\', false);
                    }
                    t.push(i, c, false);
                }
                QuoteStyle::Single => t.push(i, c, false),
            }
            continue;
        }
        match quote {
            QuoteStyle::Single => match c {
                '\'' => {
                    quote = QuoteStyle::None;
                    if let Some(t) = cur.as_mut() {
                        t.last_close = Some((open_quote_idx, i, QuoteStyle::Single));
                    }
                }
                _ => ensure_token(&mut cur, i, &mut pending_redirect).push(i, c, false),
            },
            QuoteStyle::Double => match c {
                '"' => {
                    quote = QuoteStyle::None;
                    if let Some(t) = cur.as_mut() {
                        t.last_close = Some((open_quote_idx, i, QuoteStyle::Double));
                    }
                }
                '\\' => escape = true,
                _ => ensure_token(&mut cur, i, &mut pending_redirect).push(i, c, false),
            },
            QuoteStyle::None => match c {
                '\\' => {
                    ensure_token(&mut cur, i, &mut pending_redirect);
                    escape = true;
                }
                '\'' => {
                    ensure_token(&mut cur, i, &mut pending_redirect);
                    quote = QuoteStyle::Single;
                    open_quote_idx = i;
                }
                '"' => {
                    ensure_token(&mut cur, i, &mut pending_redirect);
                    quote = QuoteStyle::Double;
                    open_quote_idx = i;
                }
                c if c.is_whitespace() => {
                    finish_token(&mut cur, &mut tokens_before, &mut command);
                }
                '|' | ';' | '&' => {
                    finish_token(&mut cur, &mut tokens_before, &mut command);
                    tokens_before = 0;
                    command = None;
                    pending_redirect = false;
                }
                '<' | '>' => {
                    finish_token(&mut cur, &mut tokens_before, &mut command);
                    pending_redirect = true;
                }
                _ => ensure_token(&mut cur, i, &mut pending_redirect).push(i, c, true),
            },
        }
    }

    let (start, value, dir_value_len, dir_raw_end, after_redirect, last_close, plain_mask) =
        match cur {
            Some(t) => (
                t.start,
                t.value,
                t.dir_value_len,
                t.dir_raw_end,
                t.after_redirect,
                t.last_close,
                t.plain_mask,
            ),
            None => (
                prefix.len(),
                String::new(),
                None,
                prefix.len(),
                pending_redirect,
                None,
                Vec::new(),
            ),
        };
    let closed_quote = last_close.and_then(|(open, close, style)| {
        (close >= dir_raw_end).then_some((style, open >= dir_raw_end))
    });
    CurrentToken {
        start,
        value,
        dir_value_len,
        dir_raw_end,
        quote,
        open_quote_idx,
        closed_quote,
        plain_mask,
        tokens_before,
        command,
        after_redirect,
    }
}

// ── Insert-token construction (quoting) ─────────────────────────────────

/// Build the replacement for the whole token: the user's verbatim directory
/// prefix plus the completed component escaped for the quote context.
/// Files close an open quote; directories keep it open and get a trailing `/`.
pub fn build_insert_token(tok: &CurrentToken, raw_dir: &str, name: &str, is_dir: bool) -> String {
    let mut out = String::with_capacity(raw_dir.len() + name.len() + 4);
    out.push_str(raw_dir);
    // A bare dash-leading component would parse as a flag. Anchor it as an
    // explicit relative path; quoting alone would not help.
    if raw_dir.is_empty() && name.starts_with('-') {
        out.push_str("./");
    }
    let (style, reopen) = match tok.quote {
        QuoteStyle::None => tok.closed_quote.unwrap_or((QuoteStyle::None, false)),
        open => (open, tok.open_quote_idx >= tok.dir_raw_end),
    };
    match style {
        QuoteStyle::None => out.push_str(&escape_unquoted(name)),
        QuoteStyle::Double => {
            if reopen {
                out.push('"');
            }
            out.push_str(&escape_double_quoted(name));
            if !is_dir {
                out.push('"');
            }
        }
        QuoteStyle::Single => {
            if reopen {
                out.push('\'');
            }
            out.push_str(&escape_single_quoted(name));
            if !is_dir {
                out.push('\'');
            }
        }
    }
    if is_dir {
        out.push('/');
    }
    out
}

/// Bash-ish set: over-escaping is harmless, under-escaping breaks the command.
fn needs_backslash(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t'
            | '"'
            | '\''
            | '\\'
            | '$'
            | '`'
            | '&'
            | '|'
            | ';'
            | '('
            | ')'
            | '<'
            | '>'
            | '*'
            | '?'
            | '['
            | ']'
            | '#'
            | '!'
            | '{'
            | '}'
            | '~'
    )
}

fn escape_unquoted(name: &str) -> String {
    // A control char cannot be backslash-escaped portably; single-quote it.
    if name.chars().any(char::is_control) {
        return format!("'{}'", escape_single_quoted(name));
    }
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if needs_backslash(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn escape_double_quoted(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn escape_single_quoted(name: &str) -> String {
    name.replace('\'', "'\\''")
}

// ── Completion requests and items ───────────────────────────────────────

/// A completion request, bound to the exact draft it was produced for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionRequest {
    pub id: u64,
    /// Full composer text at request time.
    pub text: String,
    /// Byte cursor at request time.
    pub cursor: usize,
    /// Session workspace root.
    pub cwd: String,
}

/// Where a candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionSource {
    File,
    Directory,
    Executable,
}

/// One deterministic completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// What the dropdown shows (directories end in `/`).
    pub display: String,
    /// The exact replacement for the token range.
    pub insert: String,
    pub source: CompletionSource,
    /// Byte range in the request text replaced by `insert`.
    pub replace_start: usize,
    pub replace_end: usize,
    /// The result set is incomplete (scan/result cap).
    pub truncated: bool,
}

/// The whole response for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub request: CompletionRequest,
    pub items: Vec<CompletionItem>,
    pub truncated: bool,
}

/// Provider caps. Ranking happens before the result cap.
const MAX_RESULTS: usize = 50;
const SCAN_CAP: usize = 1000;
const SYMLINK_STAT_BUDGET: usize = 64;
const PATH_CACHE_TTL: Duration = Duration::from_secs(60);

/// Produce candidates for one request. This is blocking filesystem work and is
/// called from a blocking thread by the event loop.
pub fn complete(request: &CompletionRequest) -> CompletionResult {
    let prefix = request
        .text
        .get(..request.cursor)
        .unwrap_or(request.text.as_str());
    let tok = parse_current_token(prefix);
    let range = (tok.start, request.cursor);

    if tok.value.starts_with('-') {
        return CompletionResult {
            request: request.clone(),
            items: Vec::new(),
            truncated: false,
        };
    }
    let first_token = tok.tokens_before == 0 && !tok.after_redirect;
    if first_token && !is_path_like(&tok.value) {
        return complete_path_command(request, &tok, range);
    }
    complete_file(request, &tok, range)
}

fn is_path_like(value: &str) -> bool {
    value.contains('/') || value == "~"
}

fn complete_path_command(
    request: &CompletionRequest,
    tok: &CurrentToken,
    range: (usize, usize),
) -> CompletionResult {
    if tok.value.is_empty() {
        return CompletionResult {
            request: request.clone(),
            items: Vec::new(),
            truncated: false,
        };
    }
    let cache = path_cache();
    let executables = cache.lock().map(|mut c| c.snapshot()).unwrap_or_default();
    let prefix = tok.value.as_str();
    let mut items = Vec::new();
    let mut truncated = false;

    let start = executables.partition_point(|e| e.as_str() < prefix);
    for name in executables.get(start..).unwrap_or(&[]).iter() {
        if !name.starts_with(prefix) {
            break;
        }
        if items.len() == MAX_RESULTS {
            truncated = true;
            break;
        }
        items.push(CompletionItem {
            display: name.clone(),
            insert: build_insert_token(tok, "", name, false),
            source: CompletionSource::Executable,
            replace_start: range.0,
            replace_end: range.1,
            truncated: false,
        });
    }
    if truncated {
        for item in &mut items {
            item.truncated = true;
        }
    }
    CompletionResult {
        request: request.clone(),
        items,
        truncated,
    }
}

struct ScoredEntry {
    name: String,
    is_dir: bool,
    tier: u8,
    score: u32,
}

fn complete_file(
    request: &CompletionRequest,
    tok: &CurrentToken,
    range: (usize, usize),
) -> CompletionResult {
    let cwd = Path::new(&request.cwd);
    let split = split_token(tok, &request.text, cwd, dirs_home().as_deref(), |name| {
        std::env::var(name).ok()
    });
    let (entries, truncated) = list_ranked_entries(&split.list_dir, split.match_prefix);
    let items = entries
        .into_iter()
        .map(|entry| CompletionItem {
            insert: build_insert_token(tok, &split.raw_dir, &entry.name, entry.is_dir),
            display: if entry.is_dir {
                format!("{}/", entry.name)
            } else {
                entry.name
            },
            source: if entry.is_dir {
                CompletionSource::Directory
            } else {
                CompletionSource::File
            },
            replace_start: range.0,
            replace_end: range.1,
            truncated,
        })
        .collect();
    CompletionResult {
        request: request.clone(),
        items,
        truncated,
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

struct SplitToken<'a> {
    list_dir: PathBuf,
    match_prefix: &'a str,
    /// Verbatim request slice kept in front of every completion.
    raw_dir: String,
}

fn split_token<'a>(
    tok: &'a CurrentToken,
    text: &str,
    cwd: &Path,
    home: Option<&Path>,
    lookup: impl Fn(&str) -> Option<String>,
) -> SplitToken<'a> {
    if tok.value == "~" && tok.dir_value_len.is_none() && tok.plain_mask.first() == Some(&true) {
        return SplitToken {
            list_dir: home.map_or_else(|| cwd.join("~"), Path::to_path_buf),
            match_prefix: "",
            raw_dir: "~/".to_owned(),
        };
    }
    let split_at = tok.dir_value_len.unwrap_or(0);
    let dir_value = tok.value.get(..split_at).unwrap_or("");
    let plain = tok.plain_mask.get(..split_at).unwrap_or(&[]);
    let expanded = expand_for_listing(dir_value, plain, home, lookup);
    let list_dir = if expanded.as_os_str().is_empty() {
        cwd.to_path_buf()
    } else if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    SplitToken {
        list_dir,
        match_prefix: tok.value.get(split_at..).unwrap_or(""),
        raw_dir: text
            .get(tok.start..tok.dir_raw_end)
            .unwrap_or("")
            .to_owned(),
    }
}

/// Expand `~/` and plain `$VAR` / `${VAR}` in the directory part, for listing
/// only. The inserted completion always preserves the user's raw spelling.
fn expand_for_listing(
    dir_value: &str,
    plain: &[bool],
    home: Option<&Path>,
    lookup: impl Fn(&str) -> Option<String>,
) -> PathBuf {
    if let (Some(rest), Some(home)) = (dir_value.strip_prefix("~/"), home)
        && plain.first() == Some(&true)
    {
        return PathBuf::from(format!(
            "{}/{}",
            home.to_string_lossy(),
            expand_vars(rest, plain.get(2..).unwrap_or(&[]), lookup)
        ));
    }
    PathBuf::from(expand_vars(dir_value, plain, lookup))
}

fn expand_vars(s: &str, plain: &[bool], lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        let plain_here = plain.get(i).copied().unwrap_or(false);
        if plain_here && bytes[i] == b'$' {
            let mut j = i + 1;
            let braced = bytes.get(j) == Some(&b'{');
            if braced {
                j += 1;
            }
            let name_start = j;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let name = &s[name_start..j];
            if braced && bytes.get(j) == Some(&b'}') {
                j += 1;
            }
            if !name.is_empty()
                && plain
                    .get(name_start..name_start + name.len())
                    .is_some_and(|m| m.iter().all(|p| *p))
                && let Some(value) = lookup(name)
            {
                out.push_str(&value);
                i = j;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn list_ranked_entries(dir: &Path, match_prefix: &str) -> (Vec<ScoredEntry>, bool) {
    let mut entries = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return (entries, false);
    };
    let show_hidden = match_prefix.starts_with('.');
    let prefix_lower = match_prefix.to_lowercase();
    let mut scanned = 0usize;
    let mut symlink_stats = 0usize;
    let mut truncated = false;
    for entry in read_dir {
        scanned += 1;
        if scanned > SCAN_CAP {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') && !show_hidden {
            continue;
        }
        let (tier, score) = match match_prefix.is_empty() {
            true => (0, 0),
            false => {
                let lower_name = name.to_lowercase();
                if name.starts_with(match_prefix) {
                    (0, 0)
                } else if lower_name.starts_with(&prefix_lower) {
                    (1, 0)
                } else if let Some(score) = fuzzy_subsequence(&lower_name, &prefix_lower) {
                    (2, score)
                } else {
                    continue;
                }
            }
        };
        let is_dir = match entry.file_type() {
            Ok(ft) if ft.is_symlink() && symlink_stats < SYMLINK_STAT_BUDGET => {
                symlink_stats += 1;
                // Follow the link: a symlinked directory must keep its `/`
                // and drill down like a real one. `DirEntry::metadata` would
                // return the link itself.
                std::fs::metadata(entry.path())
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
            }
            Ok(ft) => ft.is_dir(),
            Err(_) => false,
        };
        entries.push(ScoredEntry {
            name,
            is_dir,
            tier,
            score,
        });
    }
    entries.sort_unstable_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| b.is_dir.cmp(&a.is_dir))
            .then_with(|| a.name.cmp(&b.name))
    });
    truncated |= entries.len() > MAX_RESULTS;
    entries.truncate(MAX_RESULTS);
    (entries, truncated)
}

/// Greedy subsequence match. Returns a score (higher is better) or `None`.
fn fuzzy_subsequence(name: &str, needle: &str) -> Option<u32> {
    if needle.is_empty() {
        return Some(0);
    }
    let mut score = 0u32;
    let mut pos = 0usize;
    let mut last = 0usize;
    for need in needle.chars() {
        let found = name[pos..].char_indices().find(|(_, c)| *c == need)?;
        let at = pos + found.0;
        let gap = at.saturating_sub(last);
        score = score.saturating_add(10u32.saturating_sub(gap.min(10) as u32));
        last = at + need.len_utf8();
        pos = last;
    }
    Some(score)
}

// ── $PATH cache ─────────────────────────────────────────────────────────

struct PathCache {
    executables: Vec<String>,
    updated_at: Instant,
    path_env: String,
}

impl PathCache {
    fn snapshot(&mut self) -> Vec<String> {
        let current = std::env::var("PATH").unwrap_or_default();
        if self.updated_at.elapsed() < PATH_CACHE_TTL && self.path_env == current {
            return self.executables.clone();
        }
        self.executables = scan_path_from(&current);
        self.updated_at = Instant::now();
        self.path_env = current;
        self.executables.clone()
    }
}

fn path_cache() -> &'static Mutex<PathCache> {
    static CACHE: OnceLock<Mutex<PathCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(PathCache {
            executables: Vec::new(),
            updated_at: Instant::now() - PATH_CACHE_TTL - Duration::from_secs(1),
            path_env: String::new(),
        })
    })
}

/// Scan every directory in `path_var` for regular, executable files.
fn scan_path_from(path_var: &str) -> Vec<String> {
    let mut executables = Vec::new();
    for dir in std::env::split_paths(path_var) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            if let Ok(name) = entry.file_name().into_string() {
                executables.push(name);
            }
        }
    }
    executables.sort_unstable();
    executables.dedup();
    executables
}

// ── Tab decisions and menu state ───────────────────────────────────────

/// What one Tab over a completed candidate set should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabAction {
    /// Replace the token with this item (sole candidate, or menu accept).
    Accept(usize),
    /// Replace the shared span with the longer common prefix, then refetch.
    Fill(std::ops::Range<usize>, String),
    /// Show the dropdown.
    Open,
    /// No usable candidates.
    Nothing,
}

/// The dropdown's request identity and candidate set.
#[derive(Debug, Clone, Default)]
pub struct CompletionMenu {
    pub open: bool,
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub hovered: Option<usize>,
    /// The request the items were produced for.
    pub request: Option<CompletionRequest>,
    /// Painted content rect, for mouse hit-testing.
    pub rect: Option<ratatui::layout::Rect>,
    /// First item index the last frame painted (scroll window).
    pub visible_start: usize,
    /// The result was capped; never insta-accept or fill a prefix from it.
    pub truncated: bool,
    /// A fetch is in flight for this exact draft.
    pub pending: Option<CompletionRequest>,
    /// A silent refetch's candidates, waiting for the next Tab.
    pub cached: Option<CompletionResult>,
    /// Monotonic request identity.
    pub request_seq: u64,
    /// The pending fetch was armed by Tab and must run the Tab decision.
    pub tab_armed: bool,
}

impl CompletionMenu {
    pub fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.selected = 0;
        self.hovered = None;
        self.request = None;
        self.rect = None;
        self.visible_start = 0;
        self.truncated = false;
    }

    /// The draft changed: every in-flight or cached result is stale.
    pub fn reset_on_edit(&mut self) {
        self.close();
        self.pending = None;
        self.cached = None;
        self.tab_armed = false;
    }

    /// A new request for `text` at `cursor`. Repeated Tab on the same draft
    /// reuses the in-flight request instead of starting another scan.
    pub fn next_request(&mut self, text: String, cursor: usize, cwd: String) -> CompletionRequest {
        self.request_seq = self.request_seq.wrapping_add(1);
        let request = CompletionRequest {
            id: self.request_seq,
            text,
            cursor,
            cwd,
        };
        self.pending = Some(request.clone());
        request
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let len = self.items.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    pub fn page(&mut self, delta: isize, page: usize) {
        self.move_selection(delta.saturating_mul(page as isize));
    }
}

/// Terminal-Tab policy over `items`: sole -> accept; shared longer prefix ->
/// fill; otherwise open the menu. A capped scan never auto-accepts or fills.
pub fn tab_decision(result: &CompletionResult, current: &str, cursor: usize) -> TabAction {
    if result.items.is_empty() {
        return TabAction::Nothing;
    }
    if result.truncated {
        return TabAction::Open;
    }
    if result.items.len() == 1 {
        return TabAction::Accept(0);
    }
    let Some(first) = result.items.first() else {
        return TabAction::Nothing;
    };
    let range = first.replace_start..first.replace_end;
    if range.end != cursor
        || result
            .items
            .iter()
            .any(|i| i.replace_start != range.start || i.replace_end != range.end)
    {
        return TabAction::Open;
    }
    let mut lcp = first.insert.clone();
    for item in result.items.iter().skip(1) {
        lcp = common_prefix(&lcp, &item.insert).to_string();
        if lcp.is_empty() {
            return TabAction::Open;
        }
    }
    // A byte LCP can end mid-escape (`a\`); filling it would be a line
    // continuation. Trim the incomplete escape and decide again.
    while lcp.bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1 {
        lcp.pop();
    }
    let typed = current.get(range.clone()).unwrap_or("");
    if lcp.len() > typed.len() && lcp.starts_with(typed) {
        TabAction::Fill(range, lcp)
    } else {
        TabAction::Open
    }
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let mut end = 0;
    for ((ai, ac), (bi, bc)) in a.char_indices().zip(b.char_indices()) {
        if ai != bi || ac != bc {
            break;
        }
        end = ai + ac.len_utf8();
    }
    &a[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(prefix: &str) -> CurrentToken {
        parse_current_token(prefix)
    }

    #[test]
    fn parse_after_pipe_and_semicolon() {
        let t = tok("echo hi | cat foo");
        assert_eq!(t.value, "foo");
        assert_eq!(t.start, 14);
        assert_eq!(t.command.as_deref(), Some("cat"));

        let t = tok("cd /tmp; ls ");
        assert_eq!(t.value, "");
        assert_eq!(t.start, 12);
        assert_eq!(t.command.as_deref(), Some("ls"));
    }

    #[test]
    fn parse_quoted_pipe_is_one_token() {
        let t = tok("echo \"a | b");
        assert_eq!(t.value, "a | b");
        assert_eq!(t.start, 5);
        assert_eq!(t.quote, QuoteStyle::Double);
        assert_eq!(t.command.as_deref(), Some("echo"));
    }

    #[test]
    fn parse_backslash_escaped_space() {
        let t = tok("cat My\\ Fi");
        assert_eq!(t.value, "My Fi");
        assert_eq!(t.start, 4);
        assert_eq!(t.quote, QuoteStyle::None);
    }

    #[test]
    fn parse_closed_quote_dir_prefix() {
        let t = tok("cat \"My Dir\"/fi");
        assert_eq!(t.value, "My Dir/fi");
        assert_eq!(t.dir_raw_end, 13);
        assert_eq!(t.dir_value_len, Some(7));
        assert_eq!(t.closed_quote, None);
    }

    #[test]
    fn parse_redirect_target() {
        let t = tok("echo hi > lo");
        assert_eq!(t.value, "lo");
        assert!(t.after_redirect);
        assert_eq!(t.command.as_deref(), Some("echo"));
    }

    #[test]
    fn parse_multibyte_whitespace() {
        let t = tok("cat\u{3000}foo");
        assert_eq!(t.value, "foo");
    }

    #[test]
    fn parse_plain_mask_tracks_quote_and_escape() {
        let t = tok("cat '$A'/b\\$c");
        assert_eq!(t.value, "$A/b$c");
        assert_eq!(t.plain_mask, [false, false, true, true, false, true]);
    }

    #[test]
    fn escape_matrix() {
        assert_eq!(escape_unquoted("My File.txt"), "My\\ File.txt");
        assert_eq!(escape_unquoted("a;b&c|d$e`f"), "a\\;b\\&c\\|d\\$e\\`f");
        assert_eq!(escape_unquoted("a\"b'c"), "a\\\"b\\'c");
        assert_eq!(escape_double_quoted("a\"b$c"), "a\\\"b\\$c");
        assert_eq!(escape_single_quoted("it's"), "it'\\''s");
        assert_eq!(escape_unquoted("a\nb"), "'a\nb'");
    }

    #[test]
    fn insert_token_respects_quotes_and_dirs() {
        let t = tok("cat \"My Fi");
        assert_eq!(
            build_insert_token(&t, "", "My File.txt", false),
            "\"My File.txt\""
        );
        let t = tok("cat \"No");
        assert_eq!(
            build_insert_token(&t, "", "Notes Archive", true),
            "\"Notes Archive/"
        );
        let t = tok("cat 'My Fi");
        assert_eq!(
            build_insert_token(&t, "", "My File.txt", false),
            "'My File.txt'"
        );
        let t = tok("cat \"My Dir/fi\"");
        assert_eq!(
            build_insert_token(&t, "\"My Dir/", "file.txt", false),
            "\"My Dir/file.txt\""
        );
    }

    #[test]
    fn insert_token_anchors_dash_leading_names() {
        let t = tok("rm ");
        assert_eq!(build_insert_token(&t, "", "-rf", false), "./-rf");
        assert_eq!(
            build_insert_token(&t, "", "-flag dir", true),
            "./-flag\\ dir/"
        );
    }

    #[test]
    fn common_prefix_fill_requires_strict_extension() {
        let req = CompletionRequest {
            id: 1,
            text: "cat re".into(),
            cursor: 6,
            cwd: "/tmp".into(),
        };
        let mk = |insert: &str| CompletionItem {
            display: insert.into(),
            insert: insert.into(),
            source: CompletionSource::File,
            replace_start: 4,
            replace_end: 6,
            truncated: false,
        };
        let result = CompletionResult {
            request: req.clone(),
            items: vec![mk("review.md"), mk("review.txt")],
            truncated: false,
        };
        assert_eq!(
            tab_decision(&result, "cat re", 6),
            TabAction::Fill(4..6, "review.".to_owned())
        );
        let result = CompletionResult {
            request: req,
            items: vec![mk("README.md"), mk("review.md")],
            truncated: false,
        };
        assert_eq!(tab_decision(&result, "cat re", 6), TabAction::Open);
    }

    #[test]
    fn truncated_results_never_insta_accept() {
        let req = CompletionRequest {
            id: 1,
            text: "gi".into(),
            cursor: 2,
            cwd: "/tmp".into(),
        };
        let result = CompletionResult {
            request: req,
            items: vec![CompletionItem {
                display: "git".into(),
                insert: "git".into(),
                source: CompletionSource::Executable,
                replace_start: 0,
                replace_end: 2,
                truncated: true,
            }],
            truncated: true,
        };
        assert_eq!(tab_decision(&result, "gi", 2), TabAction::Open);
    }

    #[test]
    fn fuzzy_subsequence_matches_in_order() {
        assert!(fuzzy_subsequence("readme.md", "rdm").is_some());
        assert!(fuzzy_subsequence("readme.md", "xyz").is_none());
    }

    #[test]
    fn path_scan_finds_only_executables() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-shell-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("my-tool");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let data = dir.join("data.txt");
        std::fs::write(&data, "x").unwrap();
        let found = scan_path_from(&dir.to_string_lossy());
        assert!(found.contains(&"my-tool".to_string()));
        assert!(!found.contains(&"data.txt".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "leveler-shell-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn req(text: &str, cursor: usize, cwd: &Path) -> CompletionRequest {
        CompletionRequest {
            id: 1,
            text: text.into(),
            cursor,
            cwd: cwd.to_string_lossy().into_owned(),
        }
    }

    #[test]
    fn spaces_quotes_chinese_and_dashes_are_safe() {
        let dir = temp_dir("names");
        std::fs::write(dir.join("my file.txt"), "x").unwrap();
        std::fs::write(dir.join("中文.txt"), "x").unwrap();
        std::fs::write(dir.join("-rf"), "x").unwrap();

        let out = complete(&req("cat my\\ f", 8, &dir));
        assert_eq!(out.items[0].insert, "my\\ file.txt");

        let out = complete(&req("cat \"my f", 8, &dir));
        assert_eq!(out.items[0].insert, "\"my file.txt\"");

        let out = complete(&req("cat 中", 6, &dir));
        assert_eq!(out.items[0].insert, "中文.txt");

        let out = complete(&req("rm ", 3, &dir));
        assert!(
            out.items.iter().any(|i| i.insert == "./-rf"),
            "{:?}",
            out.items
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hidden_entries_need_a_dot_prefix_and_dirs_keep_their_slash() {
        let dir = temp_dir("hidden");
        std::fs::write(dir.join("visible.txt"), "x").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        std::fs::create_dir(dir.join("src")).unwrap();

        let out = complete(&req("cat ", 4, &dir));
        assert!(out.items.iter().all(|i| !i.display.starts_with('.')));
        assert!(
            out.items.iter().any(|i| i.insert == "src/"),
            "{:?}",
            out.items
        );
        let out = complete(&req("cat .", 5, &dir));
        assert!(out.items.iter().any(|i| i.insert == ".hidden"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_capped_scan_reports_truncation_and_never_claims_completeness() {
        let dir = temp_dir("large");
        for n in 0..1200 {
            std::fs::write(dir.join(format!("file-{n:04}")), "x").unwrap();
        }
        let out = complete(&req("cat file", 8, &dir));
        assert!(!out.items.is_empty());
        assert!(out.truncated, "a capped scan is never exhaustive");
        assert!(out.items.len() <= MAX_RESULTS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_unreadable_directory_returns_nothing() {
        let dir = temp_dir("missing");
        let missing = dir.join("nope");
        let out = complete(&req("cat ", 4, &missing));
        assert!(out.items.is_empty());
        assert!(!out.truncated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlinked_directory_completes_with_a_slash() {
        let dir = temp_dir("symlink");
        std::fs::create_dir(dir.join("real")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        #[cfg(unix)]
        {
            let out = complete(&req("cat li", 6, &dir));
            assert!(
                out.items.iter().any(|i| i.insert == "link/"),
                "{:?}",
                out.items
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn directory_ranking_prefers_exact_then_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "leveler-shell-rank-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("review-dir")).unwrap();
        std::fs::write(dir.join("review.md"), "x").unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        let (entries, truncated) = list_ranked_entries(&dir, "re");
        assert!(!truncated);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names[0], "review-dir", "directories sort first: {names:?}");
        assert!(names.contains(&"review.md"));
        assert!(names.contains(&"README.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
