//! Bounded relation facts for a diff-shaped review object.
//!
//! The reviewer has `find_symbol`/`find_references`, but a real run showed the
//! model may never call them: an available tool is not a used one. This module
//! gives the FIRST request a small, deterministic set of facts derived from
//! the change itself — the symbols named by hunk sections and definition
//! lines, and where those names appear elsewhere in the same object.
//!
//! It is deliberately not a call graph and not a semantic index. Matching is a
//! whole-word text scan (`git grep`), so dynamic dispatch, reflection,
//! generated code and untracked files are outside it. Every block says so, and
//! the reviewer's own tools remain available for a full answer.
//!
//! Only the manual-review path calls this. A project review has no diff-shaped
//! object and gets no prefetch; the model chooses what to investigate there.

use std::path::Path;

/// Changed symbols per review. ReviewGate measured ten as useful; eight keeps
/// the block small enough to leave room for the reviewer's own reading.
const MAX_SYMBOLS: usize = 8;
/// Of the symbol budget, this many slots are reserved for calls the change
/// makes into another source file (the cross-module relation).
const MAX_CALLED_SYMBOLS: usize = 4;
/// Bounded candidate calls inspected with `git grep`.
const MAX_CALLED_CANDIDATES: usize = 48;
/// Displayed matches per symbol.
const MAX_LOCS_PER_SYMBOL: usize = 6;
/// Lines read from one `git grep`. A common name in a large repository must
/// not become an unbounded read.
const MAX_GREP_LINES: usize = 400;
/// The whole rendered block.
const MAX_BLOCK_BYTES: usize = 8 * 1024;
const MAX_SYMBOL_BYTES: usize = 80;
const MAX_LINE_BYTES: usize = 240;

pub(super) struct PrefetchOutcome {
    /// The rendered section, already headed. `None` when nothing could be
    /// derived or the scope is not diff-shaped.
    pub block: Option<String>,
    /// Facts the reviewer must know about the block's limits.
    pub limitations: Vec<String>,
}

/// Render relation facts for `diff`. `revision` is the exact commit whose tree
/// the facts must come from; `None` means the live worktree.
pub(super) fn changed_symbol_relations(
    repo: &Path,
    diff: Option<&str>,
    diff_truncated: bool,
    revision: Option<&str>,
    untracked_changed: bool,
) -> PrefetchOutcome {
    let Some(diff) = diff.filter(|diff| !diff.trim().is_empty()) else {
        return PrefetchOutcome {
            block: None,
            limitations: Vec::new(),
        };
    };
    let mut limitations = Vec::new();
    if diff_truncated {
        limitations.push("diff 在提取前已被截断，符号集合可能不完整。".to_string());
    }
    if untracked_changed {
        limitations.push(
            "未跟踪的变更文件不在工作区文本扫描内（git grep 只读已跟踪文件）；\
             它们的关系事实缺失。"
                .to_string(),
        );
    }
    if let Some(revision) = revision {
        limitations.push(format!(
            "关系事实来自固定的历史对象 `{revision}`（git grep 该对象树），不是当前工作区。"
        ));
    }
    let (mut symbols, extensions, mut called) = extract_changed_symbols(diff);
    if symbols.is_empty() && called.is_empty() {
        limitations.push(
            "未能从 diff 中提取变更符号：没有可用的 hunk 段头或定义行；\
             这不表示不存在跨模块关系。"
                .to_string(),
        );
        return PrefetchOutcome {
            block: None,
            limitations,
        };
    }
    if extensions.is_empty() {
        limitations.push("diff 中的改动不在可扫描的源码文件里，已跳过关系预取。".to_string());
        return PrefetchOutcome {
            block: None,
            limitations,
        };
    }
    // A changed line that calls a name used in ANOTHER source file is a
    // cross-module relation the reviewer would otherwise have to discover by
    // opening that module. Keep at most two, ranked by how many other source
    // files use the name.
    let mut cross: Vec<(bool, usize, usize, ChangedSymbol)> = Vec::new();
    called.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            // Among equally frequent calls, a longer name is usually a domain
            // method rather than a language/library helper. Cross-file
            // evidence still decides whether it enters the block.
            .then(right.name.len().cmp(&left.name.len()))
    });
    for candidate in called.into_iter().take(MAX_CALLED_CANDIDATES) {
        if symbols.iter().any(|symbol| symbol.name == candidate.name) {
            continue;
        }
        let Ok(hits) = grep_symbol(repo, revision, &candidate.name, &extensions) else {
            continue;
        };
        // A method the changed class itself defines and another source file
        // calls is a class-API relation, the strongest kind this scan can see.
        let defined_in_origin = hits.textual.iter().any(|hit| {
            hit.path == candidate.origin_file
                && method_declaration_symbol(&hit.text).as_deref() == Some(candidate.name.as_str())
        });
        let distinct: std::collections::BTreeSet<&str> = hits
            .textual
            .iter()
            .filter(|hit| hit.path != candidate.origin_file && !looks_like_test_path(&hit.path))
            .map(|hit| hit.path.as_str())
            .collect();
        if !distinct.is_empty() {
            cross.push((
                defined_in_origin,
                distinct.len(),
                candidate.count,
                ChangedSymbol {
                    name: candidate.name,
                    origin: format!("{} 中的调用", candidate.origin_file),
                    origin_file: Some(candidate.origin_file),
                },
            ));
        }
    }
    cross.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then(right.1.cmp(&left.1))
            .then(right.2.cmp(&left.2))
            .then(left.3.name.cmp(&right.3.name))
    });
    let call_slots = cross.len().min(MAX_CALLED_SYMBOLS);
    symbols.truncate(MAX_SYMBOLS.saturating_sub(call_slots));
    symbols.extend(
        cross
            .into_iter()
            .take(call_slots)
            .map(|(_, _, _, symbol)| symbol),
    );
    let mut body = String::from(
        "### 变更符号的代码关系（文本扫描，不是语义引用图）\n\n\
         以下事实由本次审查对象的 diff 在本地生成，用于减少前几轮取数往返。\
         匹配是整词文本扫描，**不区分读取/写入**，也不覆盖动态调用、反射、\
         框架注入或生成代码；可能不完整。需要精确答案时继续使用 \
         `find_references`/`read_file` 自行核对。\n\n",
    );
    let mut rendered = 0usize;
    for symbol in &symbols {
        let hits = match grep_symbol(repo, revision, &symbol.name, &extensions) {
            Ok(hits) => hits,
            Err(error) => {
                limitations.push(format!(
                    "`{}` 的关系扫描失败（{error}）；该符号的事实不完整。",
                    symbol.name
                ));
                continue;
            }
        };
        if hits.textual.is_empty() {
            continue;
        }
        rendered += 1;
        body.push_str(&format!("- `{}`（来自 {}）\n", symbol.name, symbol.origin));
        // Cross-file hits are the relation this block exists to expose;
        // same-file occurrences are the less surprising part.
        let (external, same): (Vec<&Hit>, Vec<&Hit>) = hits
            .textual
            .iter()
            .partition(|hit| Some(&hit.path) != symbol.origin_file.as_ref());
        for hit in external.into_iter().chain(same).take(MAX_LOCS_PER_SYMBOL) {
            body.push_str(&format!("  - {}:{}: {}\n", hit.path, hit.line, hit.text));
        }
        if hits.textual.len() > MAX_LOCS_PER_SYMBOL {
            body.push_str(&format!(
                "  - …另有 {} 处文本匹配\n",
                hits.textual.len() - MAX_LOCS_PER_SYMBOL
            ));
        }
        if hits.truncated {
            body.push_str("  - …文本匹配读取达到上限，结果不完整\n");
        }
        if !hits.test_paths.is_empty() {
            body.push_str("  - 测试路径命中: ");
            body.push_str(&hits.test_paths.join(", "));
            body.push('\n');
        }
        body.push('\n');
    }
    if rendered == 0 {
        return PrefetchOutcome {
            block: None,
            limitations,
        };
    }
    if body.len() > MAX_BLOCK_BYTES {
        body = cap(&body, MAX_BLOCK_BYTES);
        limitations.push("关系预取块超出字节上限，已截断。".to_string());
    }
    PrefetchOutcome {
        block: Some(body),
        limitations,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChangedSymbol {
    name: String,
    origin: String,
    /// The source file the symbol came from, used to show cross-file hits
    /// before same-file occurrences.
    origin_file: Option<String>,
}

/// Extract the names that describe a change: hunk-section symbols (git puts
/// the enclosing function/class there) and definitions on added/deleted lines.
/// The second value is the set of source extensions the scan may search; a
/// docs-only diff yields no source paths and therefore no relation block.
fn extract_changed_symbols(diff: &str) -> (Vec<ChangedSymbol>, Vec<String>, Vec<CalledSymbol>) {
    let mut symbols: Vec<ChangedSymbol> = Vec::new();
    let mut called: Vec<CalledSymbol> = Vec::new();
    let mut extensions = std::collections::BTreeSet::new();
    let mut current_file = String::new();
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = path.trim().to_string();
            if is_source_path(&current_file)
                && let Some((_, extension)) = current_file.rsplit_once('.')
            {
                extensions.insert(extension.to_ascii_lowercase());
            }
            in_hunk = false;
            continue;
        }
        if line.starts_with("+++ ") || line.starts_with("--- ") || line.starts_with("diff --git ") {
            if line.starts_with("+++ /dev/null") {
                current_file.clear();
            }
            in_hunk = false;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            if current_file.is_empty() || !is_source_path(&current_file) {
                continue;
            }
            if let Some(section) = hunk_section(line) {
                let origin = if current_file.is_empty() {
                    "hunk 段头".to_string()
                } else {
                    format!("{current_file} 的 hunk 段头")
                };
                for name in section_symbols(&section) {
                    push_symbol(&mut symbols, name, &origin, &current_file);
                }
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        let (code, added) = if let Some(rest) = line.strip_prefix('+') {
            if line.starts_with("+++") {
                continue;
            }
            (rest, true)
        } else if let Some(rest) = line.strip_prefix('-') {
            if line.starts_with("---") {
                continue;
            }
            (rest, false)
        } else {
            continue;
        };
        if current_file.is_empty() || !is_source_path(&current_file) {
            continue;
        }
        let origin = format!("{current_file} 的变更行");
        for name in definition_symbols(code) {
            push_symbol(&mut symbols, name, &origin, &current_file);
        }
        if added {
            // Test files call assertions and fakes; they are not the
            // production relation this block exists to expose.
            if looks_like_test_path(&current_file) {
                continue;
            }
            for name in called_symbols(code) {
                if symbols.iter().any(|symbol| symbol.name == name) {
                    continue;
                }
                if let Some(existing) = called.iter_mut().find(|candidate| candidate.name == name) {
                    existing.count = existing.count.saturating_add(1);
                    continue;
                }
                called.push(CalledSymbol {
                    name,
                    origin_file: current_file.clone(),
                    count: 1,
                });
            }
        }
    }
    symbols.truncate(MAX_SYMBOLS);
    (symbols, extensions.into_iter().collect(), called)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CalledSymbol {
    name: String,
    origin_file: String,
    /// How many changed lines call the name; higher is more central to the
    /// change and gets the bounded `git grep` budget first.
    count: usize,
}

fn push_symbol(symbols: &mut Vec<ChangedSymbol>, name: String, origin: &str, origin_file: &str) {
    if symbols.len() >= MAX_SYMBOLS
        || name.len() > MAX_SYMBOL_BYTES
        || name.chars().count() < 3
        || symbols.iter().any(|existing| existing.name == name)
    {
        return;
    }
    symbols.push(ChangedSymbol {
        name,
        origin: origin.to_string(),
        origin_file: (!origin_file.is_empty()).then(|| origin_file.to_string()),
    });
}

/// Text after the second `@@` of a hunk header, which Git fills with the
/// enclosing declaration when it can.
fn hunk_section(header: &str) -> Option<String> {
    let after_first = &header[header.find("@@")? + 2..];
    let second = after_first.find("@@")?;
    let section = after_first[second + 2..].trim();
    (!section.is_empty()).then(|| section.to_string())
}

fn section_symbols(section: &str) -> Vec<String> {
    let definitions = definition_symbols(section);
    if !definitions.is_empty() {
        return definitions;
    }
    // A Java/C# hunk section often has no `fn`/`class` keyword before the
    // method name (`public void terminate() {`). The identifier immediately
    // before `(` is the useful probe there.
    identifier_before_paren(section).into_iter().collect()
}

fn definition_symbols(code: &str) -> Vec<String> {
    const KEYWORDS: &[&str] = &[
        "fn",
        "struct",
        "enum",
        "trait",
        "class",
        "interface",
        "def",
        "func",
        "function",
        "type",
        "impl",
    ];
    let mut out = Vec::new();
    for keyword in KEYWORDS {
        let mut from = 0usize;
        while let Some(relative) = code[from..].find(keyword) {
            let at = from + relative;
            let before_ok = code[..at]
                .chars()
                .next_back()
                .is_none_or(|character| !is_ident(character));
            let after = &code[at + keyword.len()..];
            let after_ok = after
                .chars()
                .next()
                .is_none_or(|character| !is_ident(character));
            if before_ok && after_ok {
                let rest = after.trim_start();
                if let Some(name) = take_identifier(rest)
                    && looks_like_name(&name)
                    && !out.contains(&name)
                {
                    out.push(name);
                }
            }
            from = at + keyword.len();
            if from >= code.len() {
                break;
            }
        }
    }
    // Java/C# hunk sections often name only the class. A new METHOD on an
    // added line is still a changed symbol (`public Order terminate(...) {`),
    // even though no language keyword precedes it.
    if let Some(name) = method_declaration_symbol(code)
        && !out.contains(&name)
    {
        out.push(name);
    }
    out
}

/// A method/function declaration without a `fn`/`def` keyword. Restricted to
/// lines with a body opener (`{`) and a declaration-shaped prefix, so an
/// ordinary call (`checkAndSettleOrder(id);`) is not promoted to a definition.
fn method_declaration_symbol(code: &str) -> Option<String> {
    const CONTROL: &[&str] = &[
        "if",
        "for",
        "while",
        "switch",
        "catch",
        "return",
        "new",
        "throw",
        "synchronized",
        "assert",
        "match",
        "when",
        "else",
        "do",
        "typeof",
        "sizeof",
        "await",
        "yield",
        "super",
        "this",
    ];
    let trimmed = code.trim();
    if trimmed.starts_with('@')
        || trimmed.starts_with("//")
        || trimmed.starts_with('*')
        || !(trimmed.ends_with('{') || trimmed.ends_with(',') || trimmed.ends_with('('))
    {
        return None;
    }
    let name = identifier_before_paren(trimmed)?;
    if CONTROL.contains(&name.as_str()) {
        return None;
    }
    let position = trimmed.find(&name)?;
    let prefix = trimmed[..position].trim();
    if prefix.is_empty() || prefix.contains('.') || prefix.contains('=') {
        return None;
    }
    // A bare `foo(x) {` has one token before the name; a declaration such as
    // `public void foo(` or `Order foo(` has at least two or a modifier.
    let tokens: Vec<&str> = prefix.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }
    Some(name)
}

fn identifier_before_paren(code: &str) -> Option<String> {
    const CONTROL: &[&str] = &[
        "if",
        "for",
        "while",
        "switch",
        "catch",
        "return",
        "new",
        "throw",
        "synchronized",
        "assert",
        "match",
        "when",
        "else",
        "do",
        "typeof",
        "sizeof",
        "await",
        "yield",
        "super",
        "this",
    ];
    let bytes = code.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'(' {
            continue;
        }
        let mut end = index;
        while end > 0 && code[..end].ends_with(|character: char| character.is_whitespace()) {
            end -= 1;
        }
        let mut start = end;
        while start > 0 {
            let character = code[..start].chars().next_back()?;
            if is_ident(character) {
                start -= character.len_utf8();
            } else {
                break;
            }
        }
        let name = &code[start..end];
        if looks_like_name(name) && !CONTROL.contains(&name) {
            return Some(name.to_string());
        }
    }
    None
}

fn take_identifier(text: &str) -> Option<String> {
    let mut out = String::new();
    for character in text.chars() {
        if is_ident(character) {
            out.push(character);
        } else {
            break;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Every identifier immediately before a `(`, deduplicated per line. This is
/// a text scan, not a parser: a method call and a declaration look the same
/// here, so the caller filters by where the name is also used.
fn called_symbols(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (index, _) in code.match_indices('(') {
        let mut end = index;
        while end > 0 && code[..end].ends_with(|character: char| character.is_whitespace()) {
            end -= 1;
        }
        let mut start = end;
        while start > 0 {
            let Some(character) = code[..start].chars().next_back() else {
                break;
            };
            if is_ident(character) {
                start -= character.len_utf8();
            } else {
                break;
            }
        }
        let name = &code[start..end];
        if looks_like_name(name) && !out.iter().any(|existing| existing == name) {
            out.push(name.to_string());
        }
    }
    out
}

fn looks_like_name(name: &str) -> bool {
    if name.len() < 3 || name.chars().count() > MAX_SYMBOL_BYTES {
        return false;
    }
    const EXCLUDED: &[&str] = &[
        "void",
        "int",
        "long",
        "char",
        "bool",
        "boolean",
        "float",
        "double",
        "string",
        "String",
        "Self",
        "self",
        "this",
        "super",
        "main",
        "test",
        "impl",
        "type",
        "struct",
        "class",
        "enum",
        "trait",
        "func",
        "function",
        "return",
        "pub",
        "mut",
        "let",
        "const",
        "static",
        "override",
        "public",
        "private",
        "protected",
        "final",
        "async",
        "for",
        "while",
        "switch",
        "catch",
        "new",
        "throw",
        "assert",
        "match",
        "when",
        "else",
        "await",
        "yield",
        "try",
        "using",
        "lock",
        "foreach",
        "equals",
        "hashCode",
        "toString",
        "valueOf",
        "getClass",
        "compareTo",
        "compare",
        "format",
        "parse",
        "build",
        "builder",
        "add",
        "put",
        "remove",
        "contains",
        "isEmpty",
        "size",
        "length",
        "trim",
        "substring",
        "append",
        "stream",
        "collect",
        "forEach",
        "orElse",
        "ifPresent",
        "requireNonNull",
        "checkArgument",
        "isNull",
        "nonNull",
        "asList",
        "set",
        "get",
        "setId",
        "setName",
        "getId",
        "getName",
        "findById",
        "findAll",
        "save",
        "saveAll",
        "now",
        "any",
        "verify",
        "never",
        "assertEquals",
        "assertThrows",
        "singletonList",
        "asList",
        "of",
        "info",
        "debug",
        "warn",
        "error",
        "println",
        "requireNonNull",
        "isPresent",
        "orElseThrow",
        "map",
        "filter",
        "collect",
        "stream",
        "Schema",
        "scale",
    ];
    let accessor = ["get", "set", "is", "has", "to"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_uppercase)
    });
    !EXCLUDED.contains(&name)
        && !accessor
        && !name.ends_with("Exception")
        && !name.ends_with("Error")
        && name
            .chars()
            .next()
            .is_some_and(|character| character.is_alphabetic() || character == '_')
}

fn is_ident(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn is_source_path(path: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "rs", "java", "kt", "kts", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "py", "rb", "php",
        "cs", "cpp", "cc", "cxx", "c", "h", "hpp", "swift", "scala", "dart", "lua", "ex", "exs",
    ];
    let lower = path.to_ascii_lowercase();
    lower
        .rsplit_once('.')
        .is_some_and(|(_, extension)| EXTENSIONS.contains(&extension))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    path: String,
    line: u32,
    text: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SymbolHits {
    textual: Vec<Hit>,
    test_paths: Vec<String>,
    /// More matches existed than the bounded read kept.
    truncated: bool,
}

/// Whole-word text matches in the same object the review is about. Exit code
/// 1 means "no match" and is a result; anything else is a failure the caller
/// must state.
fn grep_symbol(
    repo: &Path,
    revision: Option<&str>,
    symbol: &str,
    extensions: &[String],
) -> Result<SymbolHits, String> {
    // Defense in depth: extraction only produces identifiers, but a caller
    // must never be able to turn a name into a git option or a pathspec.
    if symbol.starts_with('-') || !looks_like_name(symbol) {
        return Err(format!("`{symbol}` is not a valid symbol name"));
    }
    if extensions.is_empty() {
        return Ok(SymbolHits::default());
    }
    let mut command = std::process::Command::new("git");
    command
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .env_clear()
        .envs(leveler_core::scrubbed_environment());
    // `core.quotepath=false` keeps non-ASCII paths readable in the evidence.
    // The pathspecs restrict the scan to the source languages present in the
    // diff, so plans, docs and lock files do not become "references".
    command.args([
        "-c",
        "core.quotepath=false",
        "grep",
        "-n",
        "-w",
        "-F",
        "-e",
        symbol,
    ]);
    if let Some(revision) = revision {
        command.arg(revision);
    }
    command.arg("--");
    for extension in extensions {
        command.arg(format!("*.{extension}"));
    }
    let output = command
        .output()
        .map_err(|error| format!("git grep 无法启动: {error}"))?;
    match output.status.code() {
        Some(0) => {}
        Some(1) => return Ok(SymbolHits::default()),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "git grep 退出状态 {:?}: {}",
                output.status.code(),
                stderr.trim()
            ));
        }
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let revision_prefix = revision.map(|revision| format!("{revision}:"));
    let mut hits = SymbolHits::default();
    for line in text.lines().take(MAX_GREP_LINES) {
        // `git grep <tree>` prefixes every line with `<tree>:`. Strip only
        // the object we asked for, never an arbitrary leading segment.
        let line = match revision_prefix.as_deref() {
            Some(prefix) => line.strip_prefix(prefix).unwrap_or(line),
            None => line,
        };
        let mut parts = line.splitn(3, ':');
        let (Some(path), Some(line_number), Some(content)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(line_number) = line_number.parse::<u32>() else {
            continue;
        };
        hits.textual.push(Hit {
            path: path.to_string(),
            line: line_number,
            text: cap(content.trim(), MAX_LINE_BYTES),
        });
        if looks_like_test_path(path) && !hits.test_paths.iter().any(|seen| seen == path) {
            hits.test_paths.push(path.to_string());
            hits.test_paths.sort();
            hits.test_paths.truncate(5);
        }
    }
    if text.lines().count() > MAX_GREP_LINES {
        hits.truncated = true;
    }
    Ok(hits)
}

fn looks_like_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_test.go")
        || lower.ends_with("test.java")
        || lower.ends_with("tests.java")
        || lower.contains(".test.")
        || lower.contains(".spec.")
}

fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let cut = leveler_core::floor_char_boundary(text, max);
    format!("{}…", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    fn repo() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]);
        git(directory.path(), &["config", "user.email", "t@t"]);
        git(directory.path(), &["config", "user.name", "t"]);
        std::fs::write(
            directory.path().join("order.rs"),
            "fn terminate() {}\nfn settle() { terminate(); }\n",
        )
        .unwrap();
        std::fs::create_dir_all(directory.path().join("tests")).unwrap();
        std::fs::write(
            directory.path().join("tests/order_test.rs"),
            "fn t() { settle(); }\n",
        )
        .unwrap();
        git(directory.path(), &["add", "."]);
        git(directory.path(), &["commit", "-qm", "base"]);
        directory
    }

    #[test]
    fn changed_definitions_and_hunk_sections_are_extracted() {
        let (symbols, _, _) = extract_changed_symbols(
            "diff --git a/order.rs b/order.rs\n\
             --- a/order.rs\n\
             +++ b/order.rs\n\
             @@ -1,1 +1,2 @@ fn settle_order() {\n\
             +fn terminate_order() {}\n",
        );
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert!(names.contains(&"settle_order"), "{names:?}");
        assert!(names.contains(&"terminate_order"), "{names:?}");
    }

    #[test]
    fn a_java_hunk_section_yields_the_method_name() {
        let (symbols, _, _) = extract_changed_symbols(
            "diff --git a/Order.java b/Order.java\n\
             --- a/Order.java\n\
             +++ b/Order.java\n\
             @@ -10,2 +10,3 @@ public void terminateSettlementReview(String reason) {\n\
             +        terminatedAt = now();\n",
        );
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert!(names.contains(&"terminateSettlementReview"), "{names:?}");
    }

    #[test]
    fn a_java_method_declaration_is_a_symbol_but_an_ordinary_call_is_not() {
        let (symbols, _, _) = extract_changed_symbols(
            "diff --git a/BillAppService.java b/BillAppService.java\n\
             --- a/BillAppService.java\n\
             +++ b/BillAppService.java\n\
             @@ -1,1 +1,3 @@ public class BillAppService {\n\
             +    public void confirmSettlement(Long billId) {\n\
             +        checkAndSettleOrder(billId);\n\
             +    }\n",
        );
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert!(names.contains(&"confirmSettlement"), "{names:?}");
        assert!(
            !names.contains(&"checkAndSettleOrder"),
            "a call is not a changed definition: {names:?}"
        );
    }

    #[test]
    fn a_multiline_java_signature_is_a_changed_symbol() {
        let (symbols, _, _) = extract_changed_symbols(
            "diff --git a/BillAppService.java b/BillAppService.java\n\
             --- a/BillAppService.java\n\
             +++ b/BillAppService.java\n\
             @@ -1,1 +1,2 @@ public class BillAppService {\n\
             +    public BillDTO submitPaymentProofs(Long billId,\n",
        );
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert!(names.contains(&"submitPaymentProofs"), "{names:?}");
    }

    #[test]
    fn a_cross_file_call_relation_is_rendered_before_same_file_hits() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]);
        git(directory.path(), &["config", "user.email", "t@t"]);
        git(directory.path(), &["config", "user.name", "t"]);
        std::fs::write(
            directory.path().join("order.rs"),
            "fn helper() {}\nfn run() {}\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("other.rs"),
            "fn use_it() { helper(); }\n",
        )
        .unwrap();
        git(directory.path(), &["add", "."]);
        git(directory.path(), &["commit", "-qm", "base"]);
        let diff = "diff --git a/order.rs b/order.rs\n\
                    --- a/order.rs\n\
                    +++ b/order.rs\n\
                    @@ -2,1 +2,2 @@ fn run() {\n\
                    +    helper();\n";
        let outcome = changed_symbol_relations(directory.path(), Some(diff), false, None, false);
        let block = outcome.block.expect("relations");
        assert!(block.contains("`helper`"), "{block}");
        let external = block.find("other.rs:1").expect("external caller");
        let same = block.find("order.rs:1").expect("same-file hit");
        assert!(
            external < same,
            "the cross-file relation must come first: {block}"
        );
    }

    #[test]
    fn relation_scan_reports_text_matches_and_test_paths() {
        let directory = repo();
        let diff = "diff --git a/order.rs b/order.rs\n\
                    --- a/order.rs\n\
                    +++ b/order.rs\n\
                    @@ -1,1 +1,2 @@ fn settle() {\n\
                    +    terminate();\n";
        let outcome = changed_symbol_relations(directory.path(), Some(diff), false, None, false);
        let block = outcome.block.expect("relations");
        assert!(block.contains("`settle`"), "{block}");
        assert!(block.contains("order.rs:2"), "{block}");
        assert!(block.contains("tests/order_test.rs"), "{block}");
        assert!(block.contains("不区分读取/写入"), "{block}");
        assert!(block.contains("可能不完整"), "{block}");
    }

    #[test]
    fn a_fixed_revision_reads_only_that_tree() {
        let directory = repo();
        let base = leveler_core::git_stdout(directory.path(), &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(
            directory.path().join("order.rs"),
            "fn terminate() {}\nfn settle() { terminate(); }\nfn later() {}\n",
        )
        .unwrap();
        git(directory.path(), &["add", "."]);
        git(directory.path(), &["commit", "-qm", "later"]);
        let diff = "diff --git a/order.rs b/order.rs\n\
                    --- a/order.rs\n\
                    +++ b/order.rs\n\
                    @@ -1,1 +1,2 @@ fn settle() {\n\
                    +    terminate();\n";
        let outcome = changed_symbol_relations(
            directory.path(),
            Some(diff),
            false,
            Some(base.trim()),
            false,
        );
        let block = outcome.block.expect("relations");
        assert!(
            !block.contains("later"),
            "the fixed object must not see the later worktree: {block}"
        );
    }

    #[test]
    fn no_diff_is_a_silent_no_op_but_an_empty_symbol_set_is_disclosed() {
        let directory = repo();
        let none = changed_symbol_relations(directory.path(), None, false, None, false);
        assert!(none.block.is_none());
        assert!(none.limitations.is_empty());

        let empty = changed_symbol_relations(
            directory.path(),
            Some("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n+a\n"),
            false,
            None,
            true,
        );
        assert!(empty.block.is_none());
        assert!(
            empty
                .limitations
                .iter()
                .any(|line| line.contains("未能从 diff"))
        );
        assert!(empty.limitations.iter().any(|line| line.contains("未跟踪")));
    }

    #[test]
    fn a_symbol_cannot_become_a_git_option() {
        let directory = repo();
        let error =
            grep_symbol(directory.path(), None, "--exec=evil", &["rs".to_string()]).unwrap_err();
        assert!(error.contains("not a valid symbol"), "{error}");
        assert!(grep_symbol(directory.path(), None, "settle", &["rs".to_string()]).is_ok());
    }
}
