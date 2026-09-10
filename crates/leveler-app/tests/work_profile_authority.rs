//! One durable fact, one reader: the session row owns a session's work
//! profile.
//!
//! `/work-mode economy` writes the row. The interactive turn used to compose
//! its tool surface from this process's create-time default instead, so the
//! row said Economy and the next turn still advertised the balanced surface.
//! Two readers of one durable fact, and the one nobody was looking at won.
//!
//! A source scan, because the defect was a CALL SITE, not a function: the
//! resolution helper can be right while a run path bypasses it. This trips
//! the moment a second reader appears.

use std::path::Path;

/// The only two places this process's own work profile may be read.
///
/// - `insert_session` stamps it onto a NEW session's row. That is what a
///   create-time default is for.
/// - `turn_axes` falls back to it for a session id with no row at all, which
///   is the only case where there is nothing durable to read.
const ALLOWED_READERS: &[&str] = &["insert_session", "turn_axes"];

#[test]
fn only_session_creation_and_the_missing_row_fallback_read_the_process_default() {
    let source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session.rs"))
            .expect("session source must be readable");

    // Track the enclosing `fn` for each hit, so the failure names the run path
    // that started reading the wrong thing.
    let mut current_fn = String::from("<file scope>");
    let mut violations = Vec::new();
    for (number, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed
            .strip_prefix("pub(crate) async fn ")
            .or_else(|| trimmed.strip_prefix("pub async fn "))
            .or_else(|| trimmed.strip_prefix("async fn "))
            .or_else(|| trimmed.strip_prefix("pub(crate) fn "))
            .or_else(|| trimmed.strip_prefix("pub fn "))
            .or_else(|| trimmed.strip_prefix("fn "))
        {
            current_fn = rest
                .split(['(', '<'])
                .next()
                .unwrap_or(rest)
                .trim()
                .to_string();
        }
        if trimmed.starts_with("//") {
            continue;
        }
        if line.contains("self.work_profile()") && !ALLOWED_READERS.contains(&current_fn.as_str()) {
            violations.push(format!(
                "src/session.rs:{}: `{current_fn}` reads this process's work \
                 profile; a turn's profile comes from the session row \
                 (`turn_axes`)",
                number + 1
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "the session row is the single authority for a turn's tool \
         surface:\n{}",
        violations.join("\n")
    );
}
