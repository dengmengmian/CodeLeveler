//! Architecture guard for memory semantics.
//!
//! Memory identity, equivalence and lifecycle transitions must be decided by
//! structured facts: exact keys, exact values, tool receipt ids, exit codes,
//! paths and evidence kinds. This guard refuses the opposite shape: a finite
//! literal table or a substring test over free text deciding what a memory
//! means. It scans constructs, not words, so adding a new synonym list does
//! not slip through by renaming its entries.
//!
//! This is a structural screen, not a proof of intent. Code review remains the
//! owner of meaning; the guard's job is to make a new text heuristic visible
//! and to force its author to state a reason, an owner and a test.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::error::{Fail, Report};

/// Checked-in exceptions. Every block names one construct family in one
/// symbol and states why it is parsing rather than guessing.
pub const EXCEPTIONS_PATH: &str = "docs/ARCHITECTURE_GUARD.exceptions";

/// Files outside `crates/leveler-memory/src` that own memory behaviour.
const EXTRA_SCOPE: &[&str] = &[
    "crates/leveler-agent/src/experience.rs",
    "crates/leveler-agent/src/memory_extract.rs",
    "crates/leveler-agent/src/executor.rs",
    "crates/leveler-app/src/memory_consolidator.rs",
    "crates/leveler-tools/src/tools/memory.rs",
];

/// Identifiers whose return would recreate the retired JSON backend, a second
/// memory write path, or a second place that decides how memory reaches a
/// model request. They are checked across every crate, not only the memory
/// crate, because each of them lived outside it too.
const FORBIDDEN_IDENTIFIERS: &[&str] = &[
    "struct MemoryStore",
    "enum MemoryBackend",
    "MemoryBackend::",
    "MemoryService::legacy",
    "with_memory_root",
    "MemoryLifecycleOp",
    "AgentAdmitOutcome",
    "ProposeOutcome",
    "leveler_memory::MemoryStore",
    "leveler_memory::new_entry",
];

/// Crates that must reach memory through `MemoryService` only. A direct
/// `MarkdownStore` here would be a second authority over the same files.
const FACADE_ONLY_CRATES: &[&str] = &[
    "crates/leveler-tui/",
    "crates/leveler-app/",
    "crates/leveler-agent/",
    "crates/leveler-tools/",
];

/// Structural guard: the deleted backend cannot come back, and no UI or
/// runtime crate may open the Markdown store directly.
fn structural_problems(repo: &Path) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    walk_rust(&repo.join("crates"), &mut files)?;
    let apps = repo.join("apps");
    if apps.is_dir() {
        walk_rust(&apps, &mut files)?;
    }
    files.sort();
    let mut problems = Vec::new();
    for path in files {
        let relative = relative(repo, &path);
        let source =
            std::fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        // Test modules and comments are documentation of past incidents; the
        // guard is about production code paths.
        let code: String = source
            .lines()
            .take_while(|line| line.trim() != "#[cfg(test)]")
            .filter(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with("//")
            })
            .collect::<Vec<_>>()
            .join("\n");
        for needle in FORBIDDEN_IDENTIFIERS {
            if code.contains(needle) {
                problems.push(format!(
                    "{relative}: `{needle}` is retired; memory has one Markdown implementation behind MemoryService"
                ));
            }
        }
        if FACADE_ONLY_CRATES
            .iter()
            .any(|prefix| relative.starts_with(prefix))
            && code.contains("MarkdownStore")
        {
            problems.push(format!(
                "{relative}: opens MarkdownStore directly; use MemoryService so one facade owns reads, writes and authority"
            ));
        }
    }
    Ok(problems)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Construct {
    /// A literal table of words or characters drives a decision.
    Table,
    /// A substring / prefix / suffix literal drives a decision.
    TextPredicate,
    /// Set comparison or a similarity algorithm relates two texts.
    TextSet,
}

impl Construct {
    fn id(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::TextPredicate => "text-predicate",
            Self::TextSet => "text-set",
        }
    }

    fn principle(self) -> &'static str {
        match self {
            Self::Table => {
                "memory.text-table: a literal word or character table decides memory behaviour"
            }
            Self::TextPredicate => {
                "memory.text-predicate: a substring of free text decides memory behaviour"
            }
            Self::TextSet => "memory.text-set: prose tokens are compared as if they were facts",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "table" => Some(Self::Table),
            "text-predicate" => Some(Self::TextPredicate),
            "text-set" => Some(Self::TextSet),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Hit {
    file: String,
    line: usize,
    symbol: String,
    construct: Construct,
    detail: String,
}

#[derive(Clone, Debug)]
struct Exception {
    file: String,
    symbol: String,
    construct: Construct,
    hits: usize,
    reason: String,
    owner: String,
    test: String,
    line: usize,
}

/// Run the guard over `repo`. A missing scope file or a malformed exception
/// file is a failure, never an empty pass.
pub fn run(repo: &Path) -> Result<Report, Fail> {
    let files = scope_files(repo).map_err(|detail| Fail::new("ARCH_SCOPE", detail))?;
    if files.is_empty() {
        return Err(Fail::new(
            "ARCH_SCOPE",
            "no memory source files were found; refusing to report PASS",
        ));
    }

    self_check().map_err(|detail| Fail::new("ARCH_SELF_CHECK", detail))?;

    let mut hits = Vec::new();
    let mut scanned = 0usize;
    for path in &files {
        let source = std::fs::read_to_string(path)
            .map_err(|err| Fail::new("ARCH_READ", format!("{}: {err}", path.display())))?;
        let relative = relative(repo, path);
        hits.extend(scan_source(&relative, &source));
        scanned += 1;
    }

    let structural =
        structural_problems(repo).map_err(|detail| Fail::new("ARCH_STRUCTURE", detail))?;
    if !structural.is_empty() {
        return Ok(Report::with_code(
            1,
            format!(
                "arch: FAIL\nstructural: {}\n{}",
                structural.len(),
                structural.join("\n")
            ),
        ));
    }

    let exceptions_path = repo.join(EXCEPTIONS_PATH);
    let exceptions_text = std::fs::read_to_string(&exceptions_path).map_err(|err| {
        Fail::new(
            "ARCH_EXCEPTIONS",
            format!(
                "{EXCEPTIONS_PATH}: {err}; every exception needs a reason, an owner and a test"
            ),
        )
    })?;
    let exceptions = match parse_exceptions(&exceptions_text) {
        Ok(exceptions) => exceptions,
        Err(detail) => {
            return Ok(Report::with_code(
                1,
                format!("arch: FAIL\nfiles: {scanned}\n{detail}"),
            ));
        }
    };

    let mut problems = Vec::new();
    let mut used: BTreeSet<(String, String, Construct)> = BTreeSet::new();
    for exception in &exceptions {
        let key = (
            exception.file.clone(),
            exception.symbol.clone(),
            exception.construct,
        );
        let actual = hits
            .iter()
            .filter(|hit| (hit.file.clone(), hit.symbol.clone(), hit.construct) == key)
            .count();
        if actual == 0 {
            problems.push(format!(
                "{}:{}: exception for `{}` {} matches no construct (stale or misspelled file/symbol)",
                EXCEPTIONS_PATH,
                exception.line,
                exception.symbol,
                exception.construct.id()
            ));
            continue;
        }
        if actual != exception.hits {
            problems.push(format!(
                "{}:{}: exception for `{}` {} declares {} hit(s) but the guard found {actual}; review the new construct (owner {}, {})",
                EXCEPTIONS_PATH,
                exception.line,
                exception.symbol,
                exception.construct.id(),
                exception.hits,
                exception.owner,
                exception.reason
            ));
            continue;
        }
        if !test_exists(repo, &exception.test) {
            problems.push(format!(
                "{}:{}: exception for `{}` names test `{}`, which does not exist",
                EXCEPTIONS_PATH, exception.line, exception.symbol, exception.test
            ));
            continue;
        }
        used.insert(key);
    }

    for hit in &hits {
        let key = (hit.file.clone(), hit.symbol.clone(), hit.construct);
        if used.contains(&key) {
            continue;
        }
        problems.push(format!(
            "{}:{}: {} in `{}` ({})",
            hit.file,
            hit.line,
            hit.construct.principle(),
            hit.symbol,
            hit.detail
        ));
    }

    if !problems.is_empty() {
        problems.sort();
        problems.dedup();
        return Ok(Report::with_code(
            1,
            format!(
                "arch: FAIL\nfiles: {scanned}\nconstructs: {}\nexceptions: {}\n{}",
                hits.len(),
                exceptions.len(),
                problems.join("\n")
            ),
        ));
    }

    Ok(Report::ok(format!(
        "arch: PASS\nfiles: {scanned}\nconstructs: {}\nexceptions: {}",
        hits.len(),
        exceptions.len()
    )))
}

fn scope_files(repo: &Path) -> Result<Vec<PathBuf>, String> {
    let root = repo.join("crates/leveler-memory/src");
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let mut files = Vec::new();
    walk_rust(&root, &mut files)?;
    for extra in EXTRA_SCOPE {
        let path = repo.join(extra);
        if !path.is_file() {
            return Err(format!(
                "{extra} is missing; update the guard's scope explicitly"
            ));
        }
        files.push(path);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn walk_rust(directory: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(directory).map_err(|err| format!("{}: {err}", directory.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("{}: {err}", directory.display()))?;
        let path = entry.path();
        if path.is_dir() {
            walk_rust(&path, out)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn relative(repo: &Path, path: &Path) -> String {
    path.strip_prefix(repo)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Scan one source file. Test modules may use arbitrary words; they are not
/// business logic and every exception still needs a test of its own.
fn scan_source(file: &str, source: &str) -> Vec<Hit> {
    let all: Vec<&str> = source.lines().collect();
    let end = all
        .iter()
        .position(|line| line.trim() == "#[cfg(test)]")
        .unwrap_or(all.len());
    let lines = &all[..end];
    let mut hits = Vec::new();
    let mut symbol = String::from("<top-level>");

    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        if let Some(name) = function_name(line) {
            symbol = name;
        }

        if let Some(name) = table_start(line) {
            let mut literals = 0usize;
            let mut end_line = index;
            while end_line < lines.len() {
                literals += literal_count(lines[end_line]);
                if lines[end_line].contains("];") {
                    break;
                }
                end_line += 1;
            }
            if literals >= 2 {
                for use_line in table_uses(lines, index, end_line + 1, &name) {
                    let use_symbol = enclosing_symbol(lines, use_line.0);
                    hits.push(Hit {
                        file: file.to_string(),
                        line: use_line.0 + 1,
                        symbol: use_symbol,
                        construct: Construct::Table,
                        detail: format!("table `{name}` has {literals} literals"),
                    });
                }
            }
        }

        let char_literals = bracket_char_literals(line);
        if char_literals >= 2 && is_decision(line) {
            hits.push(Hit {
                file: file.to_string(),
                line: index + 1,
                symbol: symbol.clone(),
                construct: Construct::Table,
                detail: format!("inline table of {char_literals} character literals"),
            });
        }

        for (needle, literal) in text_predicates(line) {
            if !literal_is_text(&literal) {
                continue;
            }
            hits.push(Hit {
                file: file.to_string(),
                line: index + 1,
                symbol: symbol.clone(),
                construct: Construct::TextPredicate,
                detail: format!("{needle}(\"{literal}\")"),
            });
        }

        if let Some(name) = text_set_construct(line) {
            hits.push(Hit {
                file: file.to_string(),
                line: index + 1,
                symbol: symbol.clone(),
                construct: Construct::TextSet,
                detail: name.to_string(),
            });
        }

        index += 1;
    }
    hits
}

fn function_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('#') {
        return None;
    }
    let mut search = 0usize;
    while let Some(offset) = line[search..].find("fn ") {
        let at = search + offset;
        let boundary = at == 0
            || !line[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if boundary {
            let rest = &line[at + 3..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
        search = at + 3;
    }
    None
}

fn enclosing_symbol(lines: &[&str], at: usize) -> String {
    for line in lines[..=at].iter().rev() {
        if let Some(name) = function_name(line) {
            return name;
        }
    }
    "<top-level>".to_string()
}

fn table_start(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = if let Some(rest) = trimmed.strip_prefix("pub(crate) ") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("pub ") {
        rest
    } else {
        trimmed
    };
    let rest = if let Some(rest) = rest.strip_prefix("const ") {
        rest
    } else if let Some(rest) = rest.strip_prefix("static ") {
        rest
    } else {
        return None;
    };
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    let typed = trimmed.contains("&[&str]")
        || trimmed.contains("[&str;")
        || trimmed.contains("&[char]")
        || trimmed.contains("[char;");
    typed.then_some(name)
}

fn is_decision(line: &str) -> bool {
    [
        ".iter()",
        ".any(",
        ".all(",
        ".contains(",
        "matches!",
        ".position(",
        ".find(",
        " for ",
    ]
    .iter()
    .any(|needle| line.contains(needle))
}

fn literal_count(line: &str) -> usize {
    let mut count = 0usize;
    let bytes = line.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                count += 1;
                index += 1;
                while index < bytes.len() && bytes[index] != b'"' {
                    if bytes[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
            }
            b'\'' => {
                // A char literal is `'x'` or `'\n'`; a lifetime is not.
                let close = line[index + 1..].find('\'');
                if let Some(offset) = close
                    && offset <= 3
                {
                    count += 1;
                    index += offset + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    count
}

/// Character literals inside one bracket span on this line. A `matches!`
/// alternation or a range is protocol parsing, not a word table; only an
/// actual `[ ... ]` array is a table.
fn bracket_char_literals(line: &str) -> usize {
    let Some(open) = line.find('[') else {
        return 0;
    };
    let Some(close) = line[open..].find(']').map(|offset| open + offset) else {
        return 0;
    };
    let span = &line[open + 1..close];
    let mut count = 0usize;
    let bytes = span.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\'' && index + 1 < bytes.len() {
            let rest = &span[index + 1..];
            let width = if rest.starts_with('\\') {
                rest[1..].find('\'').map(|offset| offset + 2)
            } else {
                rest.find('\'').map(|offset| offset + 1)
            };
            if let Some(width) = width
                && width <= 4
            {
                count += 1;
                index += width;
            }
        }
        index += 1;
    }
    count
}

/// Where a declared table is actually used. A function-local table is scoped
/// to its own function; two functions may each declare `MARKERS`, and one
/// must not claim the other's use.
fn table_uses<'a>(
    lines: &'a [&'a str],
    declaration: usize,
    after: usize,
    name: &str,
) -> Vec<(usize, &'a str)> {
    let indented = lines[declaration]
        .chars()
        .next()
        .is_some_and(|c| c.is_whitespace());
    let (start, end) = if indented {
        let start = lines[..declaration]
            .iter()
            .rposition(|line| function_name(line).is_some())
            .map(|at| at + 1)
            .unwrap_or(0);
        let end = lines[declaration..]
            .iter()
            .position(|line| function_name(line).is_some())
            .map(|offset| declaration + offset)
            .unwrap_or(lines.len());
        (start, end)
    } else {
        (0, lines.len())
    };
    (start..end)
        .filter(|index| {
            *index >= after && lines[*index].contains(name) && is_decision(lines[*index])
        })
        .map(|index| (index, lines[index]))
        .collect()
}

fn text_predicates(line: &str) -> Vec<(&'static str, String)> {
    const NEEDLES: &[&str] = &[
        ".contains(\"",
        ".starts_with(\"",
        ".ends_with(\"",
        ".matches(\"",
    ];
    let mut out = Vec::new();
    for needle in NEEDLES {
        for (at, _) in line.match_indices(needle) {
            let rest = &line[at + needle.len()..];
            let mut literal = String::new();
            let mut escaped = false;
            for character in rest.chars() {
                if escaped {
                    literal.push(character);
                    escaped = false;
                    continue;
                }
                match character {
                    '\\' => escaped = true,
                    '"' => break,
                    other => literal.push(other),
                }
            }
            out.push((
                needle.trim_start_matches('.').trim_end_matches("(\""),
                literal,
            ));
        }
    }
    out
}

/// Protocol punctuation (`".."`, `"k-"` is text because it carries a letter,
/// `"---"`) is not a word. A literal must carry at least two alphabetic or
/// non-ASCII characters to be a semantic candidate.
fn literal_is_text(literal: &str) -> bool {
    let textual = literal
        .chars()
        .filter(|c| c.is_alphabetic() || !c.is_ascii())
        .count();
    textual >= 1 && literal.chars().count() >= 2
}

fn text_set_construct(line: &str) -> Option<&'static str> {
    const NEEDLES: &[&str] = &[
        ".is_subset(",
        ".intersection(",
        ".difference(",
        "jaccard",
        "levenshtein",
        "edit_distance",
        "cosine_similarity",
        "text_similarity",
    ];
    NEEDLES
        .iter()
        .find(|needle| line.contains(**needle))
        .copied()
}

fn parse_exceptions(text: &str) -> Result<Vec<Exception>, String> {
    let mut out = Vec::new();
    let mut block: BTreeMap<String, String> = BTreeMap::new();
    let mut start_line = 1usize;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "---" {
            if !block.is_empty() {
                out.push(exception_from(&block, start_line)?);
                block.clear();
            }
            start_line = index + 2;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "{}:{}: expected `key = value`",
                EXCEPTIONS_PATH,
                index + 1
            ));
        };
        let key = key.trim();
        let value = value.trim();
        if !matches!(
            key,
            "file" | "symbol" | "construct" | "hits" | "reason" | "owner" | "test"
        ) {
            return Err(format!(
                "{}:{}: unknown key `{key}`",
                EXCEPTIONS_PATH,
                index + 1
            ));
        }
        if block.insert(key.to_string(), value.to_string()).is_some() {
            return Err(format!(
                "{}:{}: duplicate key `{key}`",
                EXCEPTIONS_PATH,
                index + 1
            ));
        }
    }
    if !block.is_empty() {
        out.push(exception_from(&block, start_line)?);
    }
    let mut keys = BTreeSet::new();
    for exception in &out {
        if !keys.insert((
            exception.file.clone(),
            exception.symbol.clone(),
            exception.construct,
        )) {
            return Err(format!(
                "{}:{}: duplicate exception for `{}` {}",
                EXCEPTIONS_PATH,
                exception.line,
                exception.symbol,
                exception.construct.id()
            ));
        }
    }
    Ok(out)
}

fn exception_from(block: &BTreeMap<String, String>, line: usize) -> Result<Exception, String> {
    let required = |key: &str| -> Result<String, String> {
        block
            .get(key)
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| format!("{EXCEPTIONS_PATH}:{line}: missing `{key}`"))
    };
    let construct_raw = required("construct")?;
    let construct = Construct::parse(&construct_raw).ok_or_else(|| {
        format!(
            "{EXCEPTIONS_PATH}:{line}: construct `{construct_raw}` is not table, text-predicate or text-set"
        )
    })?;
    let hits_raw = required("hits")?;
    let hits = hits_raw
        .parse::<usize>()
        .map_err(|_| format!("{EXCEPTIONS_PATH}:{line}: hits `{hits_raw}` is not a number"))?;
    if hits == 0 {
        return Err(format!(
            "{EXCEPTIONS_PATH}:{line}: an exception with zero hits is stale"
        ));
    }
    Ok(Exception {
        file: required("file")?,
        symbol: required("symbol")?,
        construct,
        hits,
        reason: required("reason")?,
        owner: required("owner")?,
        test: required("test")?,
        line,
    })
}

fn test_exists(repo: &Path, name: &str) -> bool {
    let needle = format!("fn {name}(");
    let mut files = Vec::new();
    for root in ["crates", "xtask"] {
        let path = repo.join(root);
        if path.is_dir() {
            let _ = walk_rust(&path, &mut files);
        }
    }
    files.iter().any(|path| {
        std::fs::read_to_string(path)
            .map(|source| source.contains(&needle))
            .unwrap_or(false)
    })
}

/// The guard must prove it can still see the shapes it forbids. Without this,
/// a broken scan would report a clean bill of health.
fn self_check() -> Result<(), String> {
    let fixture = r#"
const TOPICS: &[&str] = &["billing", "invoice"];
fn decide(text: &str) -> bool {
    TOPICS.iter().any(|topic| text.contains(topic))
}
fn prefix(text: &str) -> bool {
    text.starts_with("deploy")
}
fn overlap(left: &BTreeSet<String>, right: &BTreeSet<String>) -> bool {
    left.intersection(right).count() > 0
}
"#;
    let hits = scan_source("self-check.rs", fixture);
    let found = |construct| hits.iter().any(|hit| hit.construct == construct);
    if !found(Construct::Table) {
        return Err("a literal table was not detected".to_string());
    }
    if !found(Construct::TextPredicate) {
        return Err("a text predicate was not detected".to_string());
    }
    if !found(Construct::TextSet) {
        return Err("a text set comparison was not detected".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "leveler-arch-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn repo_with(source: &str, exceptions: &str) -> TempRepo {
        let dir = TempRepo::new();
        write(&dir.path().join("crates/leveler-memory/src/lib.rs"), source);
        for extra in EXTRA_SCOPE {
            write(&dir.path().join(extra), "fn placeholder() {}\n");
        }
        write(&dir.path().join(EXCEPTIONS_PATH), exceptions);
        write(
            &dir.path().join("crates/leveler-memory/src/tests.rs"),
            "fn a_named_test() {}\n",
        );
        dir
    }

    fn report(repo: &Path) -> String {
        match run(repo) {
            Ok(report) => report.text,
            Err(fail) => fail.detail,
        }
    }

    #[test]
    fn a_reintroduced_memory_backend_fails_even_outside_the_memory_crate() {
        let dir = repo_with("fn production() {}\n", "");
        write(
            &dir.path().join("crates/leveler-agent/src/executor.rs"),
            "fn connect() { let _ = MemoryService::legacy(root); }\n",
        );
        let text = report(dir.path());
        assert!(text.contains("arch: FAIL"), "{text}");
        assert!(text.contains("MemoryService::legacy"), "{text}");
        assert!(text.contains("retired"), "{text}");
    }

    #[test]
    fn a_test_module_may_still_describe_the_old_path() {
        // The guard is about production paths; a test that pins the refusal of
        // a retired command is documentation, not a second implementation.
        let dir = repo_with("fn production() {}\n", "");
        write(
            &dir.path().join("crates/leveler-cli/src/memory_cmds.rs"),
            "fn production() {}\n#[cfg(test)]\nmod tests {\n    fn note() { let _ = MemoryService::legacy; }\n}\n",
        );
        let text = report(dir.path());
        assert!(text.contains("arch: PASS"), "{text}");
    }

    #[test]
    fn a_ui_crate_may_not_open_the_markdown_store_directly() {
        let dir = repo_with("fn production() {}\n", "");
        write(
            &dir.path().join("crates/leveler-tui/src/memory.rs"),
            "fn open() { let _ = MarkdownStore::open(roots); }\n",
        );
        let text = report(dir.path());
        assert!(text.contains("arch: FAIL"), "{text}");
        assert!(text.contains("opens MarkdownStore directly"), "{text}");
    }

    #[test]
    fn a_new_word_table_fails_with_file_line_and_principle() {
        let dir = repo_with(
            r#"
const TOPICS: &[&str] = &["billing", "invoice"];
fn decide(text: &str) -> bool {
    TOPICS.iter().any(|topic| text.contains(topic))
}
"#,
            "",
        );
        let text = report(dir.path());
        assert!(text.contains("arch: FAIL"), "{text}");
        assert!(
            text.contains("crates/leveler-memory/src/lib.rs:4"),
            "{text}"
        );
        assert!(text.contains("memory.text-table"), "{text}");
    }

    #[test]
    fn a_protocol_exception_with_reason_owner_and_test_passes() {
        let source = r#"
fn is_protocol_marker(text: &str) -> bool {
    text.starts_with("exit: ")
}
"#;
        let exceptions = format!(
            "file = crates/leveler-memory/src/lib.rs\nsymbol = is_protocol_marker\nconstruct = text-predicate\nhits = 1\nreason = tool receipt protocol, not a memory fact\nowner = runtime\ntest = a_named_test\n"
        );
        let dir = repo_with(source, &exceptions);
        let text = report(dir.path());
        assert!(text.contains("arch: PASS"), "{text}");
    }

    #[test]
    fn an_exception_whose_symbol_grows_fails() {
        let source = r#"
fn is_protocol_marker(text: &str) -> bool {
    text.starts_with("exit: ") || text.ends_with("done")
}
"#;
        let exceptions = "file = crates/leveler-memory/src/lib.rs\nsymbol = is_protocol_marker\nconstruct = text-predicate\nhits = 1\nreason = protocol\nowner = runtime\ntest = a_named_test\n";
        let dir = repo_with(source, exceptions);
        let text = report(dir.path());
        assert!(text.contains("arch: FAIL"), "{text}");
        assert!(
            text.contains("declares 1 hit(s) but the guard found 2"),
            "{text}"
        );
    }

    #[test]
    fn a_stale_exception_fails() {
        let exceptions = "file = crates/leveler-memory/src/lib.rs\nsymbol = gone\nconstruct = text-predicate\nhits = 1\nreason = protocol\nowner = runtime\ntest = a_named_test\n";
        let dir = repo_with("fn plain() {}\n", exceptions);
        let text = report(dir.path());
        assert!(text.contains("arch: FAIL"), "{text}");
        assert!(text.contains("matches no construct"), "{text}");
    }

    #[test]
    fn an_exception_without_reason_owner_or_test_fails() {
        let source = "fn marker(text: &str) -> bool { text.contains(\"deploy\") }\n";
        for missing in ["reason", "owner", "test"] {
            let mut exceptions = String::from(
                "file = crates/leveler-memory/src/lib.rs\nsymbol = marker\nconstruct = text-predicate\nhits = 1\nreason = protocol\nowner = runtime\ntest = a_named_test\n",
            );
            exceptions = exceptions
                .lines()
                .filter(|line| !line.starts_with(&format!("{missing} =")))
                .map(|line| format!("{line}\n"))
                .collect();
            let dir = repo_with(source, &exceptions);
            let text = report(dir.path());
            assert!(text.contains("arch: FAIL"), "{missing}: {text}");
            assert!(
                text.contains(&format!("missing `{missing}`")),
                "{missing}: {text}"
            );
        }
    }

    #[test]
    fn an_exception_naming_a_missing_test_fails() {
        let source = "fn marker(text: &str) -> bool { text.contains(\"deploy\") }\n";
        let exceptions = "file = crates/leveler-memory/src/lib.rs\nsymbol = marker\nconstruct = text-predicate\nhits = 1\nreason = protocol\nowner = runtime\ntest = no_such_test\n";
        let dir = repo_with(source, exceptions);
        let text = report(dir.path());
        assert!(text.contains("arch: FAIL"), "{text}");
        assert!(text.contains("does not exist"), "{text}");
    }

    #[test]
    fn test_modules_are_not_business_logic() {
        let source = r#"
#[cfg(test)]
mod tests {
    const WORDS: &[&str] = &["billing", "invoice"];
    fn decide(text: &str) -> bool { WORDS.iter().any(|word| text.contains(word)) }
}
"#;
        let dir = repo_with(source, "");
        let text = report(dir.path());
        assert!(text.contains("arch: PASS"), "{text}");
    }

    #[test]
    fn a_missing_exceptions_file_is_not_a_pass() {
        let dir = repo_with("fn plain() {}\n", "");
        std::fs::remove_file(dir.path().join(EXCEPTIONS_PATH)).unwrap();
        let text = report(dir.path());
        assert!(text.contains(EXCEPTIONS_PATH), "{text}");
        assert!(text.contains("No such file"), "{text}");
    }

    #[test]
    fn the_guard_reports_its_scope_and_counts() {
        let dir = repo_with("fn plain() {}\n", "");
        let text = report(dir.path());
        assert!(text.contains("arch: PASS"), "{text}");
        assert!(text.contains("files: 7"), "{text}");
        assert!(text.contains("constructs: 0"), "{text}");
    }

    #[test]
    fn the_checked_in_exceptions_cover_the_real_repository() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let report = run(&repo).expect("guard must run");
        assert_eq!(report.code, 0, "{}", report.text);
    }
}
