//! Ownership tripwire: runtime-authoritative canonical execution writes must
//! go through the ownership-fenced `EventLog::new_owned`, not the unfenced
//! `EventLog::new`. This scans engine and app production sources and fails
//! the moment an un-allowlisted `EventLog::new(` appears — the textual
//! counterpart of the ToolHost boundary tripwire.
//!
//! The allowlist names every remaining unfenced use and why it is legitimate
//! (definition site; read-only context assembly that never appends). Adding
//! a new canonical execution write behind `EventLog::new` requires editing
//! this list, which is the review moment the tripwire exists to force.

use std::path::{Path, PathBuf};

/// `(suffix, allowed occurrences, justification)`.
const ALLOWED: &[(&str, usize, &str)] = &[
    (
        "leveler-engine/src/log.rs",
        1,
        "the constructor's own definition",
    ),
    (
        "leveler-app/src/interactive.rs",
        1,
        "side-question context assembly: reads the latest snapshot, never appends",
    ),
    (
        "leveler-engine/src/engine.rs",
        1,
        "bounded transcript load: reads the latest snapshot's watermark to decide \
         how much history to read, before the turn's ownership token exists, and \
         never appends",
    ),
];

const SCANNED: &[&str] = &["src", "../leveler-app/src"];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("source dir readable") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn canonical_execution_writes_use_the_owned_event_log() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();
    for root in SCANNED {
        let mut files = Vec::new();
        rust_sources(&manifest.join(root), &mut files);
        for path in files {
            let source = std::fs::read_to_string(&path).expect("source readable");
            // Production code only: inline `#[cfg(test)]` modules may use the
            // unfenced constructor freely (fixtures, read-back assertions).
            let production = source
                .split("#[cfg(test)]")
                .next()
                .unwrap_or(source.as_str());
            let count = production.matches("EventLog::new(").count();
            if count == 0 {
                continue;
            }
            let display = path.display().to_string().replace('\\', "/");
            let allowed = ALLOWED
                .iter()
                .find(|(suffix, _, _)| display.ends_with(suffix));
            match allowed {
                Some((_, max, _)) if count <= *max => {}
                _ => violations.push(format!(
                    "{display}: {count} unfenced EventLog::new( occurrence(s); \
                     runtime-authoritative writes must use EventLog::new_owned \
                     (or extend the allowlist with a justification)"
                )),
            }
        }
    }
    assert!(violations.is_empty(), "\n{}", violations.join("\n"));
}
