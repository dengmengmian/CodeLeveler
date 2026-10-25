//! The model-visible markers a size-bounded tool result carries.
//!
//! A tool result that had to be reduced must say WHAT happened and WHERE the
//! rest is. The marker format lives here because it is model-visible content
//! that more than one layer must recognize:
//!
//! * `leveler-tools` writes it when a result is offloaded to the artifact
//!   store (or elided with no store);
//! * `leveler-context` recognizes it when the hard-capacity recovery has to
//!   reduce an already-externalized result to its reference;
//! * context accounting may later price each shape.
//!
//! Keeping the format in one crate is the same rule that puts
//! [`crate::COMPACTION_BREADCRUMB_MARKER`] here: a marker two layers must agree
//! on cannot be a private string in one of them.

/// Stable prefix of a result whose full content was saved to the artifact
/// store. The marker is the FIRST line of the result, so it survives every
/// head/tail cap the registry applies.
pub const TOOL_OUTPUT_SAVED_PREFIX: &str = "[tool output saved:";

/// Fragment of the registry's head/tail elision marker. A result carrying it
/// was reduced without a store, so the elided content is not recoverable.
pub const TOOL_OUTPUT_ELIDED_MARKER: &str = "elided to fit the context";

/// The reference line for a result stored at `path`. The path is absolute and
/// is the same one `read_file` accepts; the model can page it with
/// `start_line`/`end_line`.
pub fn saved_reference(path: &str, sha256: &str, size_bytes: usize) -> String {
    format!(
        "{TOOL_OUTPUT_SAVED_PREFIX} {path} | sha256:{sha256} | {size_bytes} bytes. \
         The preview below is partial; read the full output with read_file, passing \
         this absolute path and start_line to page.]"
    )
}

/// The registry writes its elision marker as a whole line:
/// `… [N bytes (~T tokens) elided to fit the context] …`. Matching the line
/// shape rather than the phrase keeps a complete result that merely mentions
/// the phrase (source code, logs) from being mistaken for a truncated one.
fn is_elision_marker_line(line: &str) -> bool {
    line.starts_with("… [")
        && line.ends_with("] …")
        && line.contains(TOOL_OUTPUT_ELIDED_MARKER)
}

/// Reduce an already-externalized tool result to what remains recoverable.
///
/// Returns `None` for a result that carries no externalization marker: its
/// content is the only copy and must never be dropped by this path.
///
/// * Saved result: keep the reference line (path, hash, size). The full
///   content is still on disk and the model can read it again.
/// * Elided-without-store result: keep an explicit statement that the full
///   output was not retained. The preview itself is what a hard capacity
///   needs to give up, and it was already partial.
pub fn reference_only(content: &str) -> Option<String> {
    if content.starts_with(TOOL_OUTPUT_SAVED_PREFIX) {
        let line = content.lines().next().unwrap_or(TOOL_OUTPUT_SAVED_PREFIX);
        return Some(format!(
            "{line}\n[The preview was reduced to its saved reference to stay within \
             the context window. The full output is at the path above.]"
        ));
    }
    if content.lines().any(is_elision_marker_line) {
        return Some(
            "[tool output truncated: the full result was not retained. Only a partial \
             preview was kept, and it has now been reduced to stay within the context \
             window. Re-run the tool if the output is needed again.]"
                .to_string(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_normal_result_is_never_reduced() {
        assert_eq!(reference_only("exit 0\nall tests passed"), None);
    }

    #[test]
    fn a_saved_result_keeps_the_reference_and_says_where_the_rest_is() {
        let content = format!(
            "{}\nHEAD…TAIL",
            saved_reference("/state/artifacts/abc.txt", "deadbeef", 200_000)
        );
        let reduced = reference_only(&content).expect("saved result is reducible");
        assert!(reduced.contains("/state/artifacts/abc.txt"));
        assert!(reduced.contains("sha256:deadbeef"));
        assert!(reduced.contains("200000 bytes"));
        assert!(!reduced.contains("HEAD…TAIL"), "the preview is given up");
        assert!(reduced.contains("full output is at the path above"));
    }

    #[test]
    fn a_complete_result_that_merely_mentions_elision_is_never_reduced() {
        // Source and logs can talk about the marker. Only the registry's own
        // whole marker line means "this result was truncated".
        let content = "the cap inserts \"elided to fit the context\" between head and tail\n\
                       fn cap() { /* … [12 bytes (~5 tokens) elided to fit the context] … */ }";
        assert_eq!(reference_only(content), None);
    }

    #[test]
    fn an_elided_result_is_reduced_to_an_explicit_loss_statement() {
        let content = "… [100000 bytes (~40000 tokens) elided to fit the context] …";
        let reduced = reference_only(content).expect("elided result is reducible");
        assert!(reduced.contains("not retained"));
        assert!(!reduced.contains("40000 tokens"));
    }
}
