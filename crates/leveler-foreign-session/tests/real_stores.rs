//! Opt-in proof against the real local Claude Code and Codex stores.
//!
//! Run explicitly:
//!
//! ```text
//! LEVELER_REAL_CLAUDE_ID=<uuid> LEVELER_REAL_CODEX_ID=<uuid> \
//!     cargo test -p leveler-foreign-session --test real_stores -- --ignored --nocapture
//! ```
//!
//! Roots default to `$HOME/.claude` and `$HOME/.codex`; override with
//! `LEVELER_REAL_CLAUDE_ROOT` / `LEVELER_REAL_CODEX_ROOT`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use leveler_foreign_session::*;

fn root(variable: &str, default_dir: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(default_dir))
}

fn source_fingerprint(path: &Path) -> (Vec<u8>, SystemTime) {
    let bytes = fs::read(path).expect("read source");
    let modified = fs::metadata(path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    (bytes, modified)
}

fn exercise(tool: ForeignTool, store: PathBuf, id: &str) {
    assert!(store.exists(), "store not found: {}", store.display());
    let started = Instant::now();
    let summary = find(tool, &store, id).expect("find real session");
    let find_ms = started.elapsed();
    assert_eq!(summary.native_id, id);
    assert!(summary.size_bytes > 0);
    assert!(summary.source_path.starts_with(&store));

    let before = source_fingerprint(&summary.source_path);

    let started = Instant::now();
    let digest = digest(&summary).expect("digest real session");
    let digest_ms = started.elapsed();
    assert!(
        digest.first_user_message.is_some() || digest.stats.assistant_messages > 0,
        "no conversation content"
    );
    assert!(!digest.early_user_messages.is_empty());

    let started = Instant::now();
    let index = index(&summary).expect("index real session");
    let index_ms = started.elapsed();
    assert!(!index.events.is_empty());
    assert!(index.events.len() <= MAX_INDEX_EVENTS);
    assert!(
        index
            .events
            .iter()
            .all(|event| event.preview.chars().count() <= 241)
    );

    let first = read_events(&summary, 1, 5).expect("read real events");
    assert!(!first.is_empty());

    let needle = digest
        .first_user_message
        .as_deref()
        .and_then(|text| text.split_whitespace().find(|word| word.len() >= 4))
        .unwrap_or("the");
    let hits = search(&summary, needle, 5).expect("search real session");
    assert!(!hits.is_empty(), "no hit for probe {needle:?}");

    let after = source_fingerprint(&summary.source_path);
    assert_eq!(after.0, before.0, "source bytes changed");
    assert_eq!(after.1, before.1, "source mtime changed");

    println!(
        "{tool:?} {}: {} bytes, find {:?}, digest {:?}, index {} events {:?}",
        summary.native_id,
        summary.size_bytes,
        find_ms,
        digest_ms,
        index.events.len(),
        index_ms
    );
    println!(
        "  cwd={:?} branch={:?} users={} assistants={} calls={} results={} errors={} compactions={} malformed={}",
        summary.cwd,
        summary.branch,
        digest.stats.user_messages,
        digest.stats.assistant_messages,
        digest.stats.tool_calls,
        digest.stats.tool_results,
        digest.stats.errors,
        digest.stats.compactions,
        digest.stats.malformed_lines
    );
    println!(
        "  first_user={:?}",
        digest
            .first_user_message
            .as_deref()
            .map(|text| text.chars().take(120).collect::<String>())
    );
}

#[test]
#[ignore = "opt-in: reads the real local Claude Code and Codex stores"]
fn real_local_stores_are_read_without_modification() {
    let Some(claude_id) = std::env::var("LEVELER_REAL_CLAUDE_ID").ok() else {
        eprintln!("LEVELER_REAL_CLAUDE_ID not set; skipping Claude Code");
        return;
    };
    exercise(
        ForeignTool::ClaudeCode,
        root("LEVELER_REAL_CLAUDE_ROOT", ".claude"),
        &claude_id,
    );

    let Some(codex_id) = std::env::var("LEVELER_REAL_CODEX_ID").ok() else {
        eprintln!("LEVELER_REAL_CODEX_ID not set; skipping Codex");
        return;
    };
    exercise(
        ForeignTool::Codex,
        root("LEVELER_REAL_CODEX_ROOT", ".codex"),
        &codex_id,
    );
}
