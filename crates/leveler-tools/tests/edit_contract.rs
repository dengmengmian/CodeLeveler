//! The `apply_patch` matching contract.
//!
//! The tool's description says the context and `-` lines must match the file
//! EXACTLY. These tests hold the implementation to that sentence, from the
//! outside, through the real filesystem write path.
//!
//! What makes this a correctness question rather than a matter of degree: a
//! located hunk is applied by splicing the model's lines over the file's, and
//! an unchanged (` `) context line is part of that splice. So a hunk located by
//! a loose comparison rewrites the file's real bytes on lines the model never
//! asked to change — a silent, unreported edit inside a call that reports
//! success.

use leveler_execution::{PermissionProfile, Workspace};
use leveler_tools::ToolContext;
use leveler_tools::default_registry;
use tokio_util::sync::CancellationToken;

fn workspace(name: &str, file: &str, content: &str) -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("leveler-edit-{name}-"))
        .tempdir()
        .unwrap();
    std::fs::write(dir.path().join(file), content).unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let ctx = ToolContext::new(ws, PermissionProfile::Assisted);
    (dir, ctx)
}

async fn patch(ctx: &ToolContext, patch: &str) -> (bool, String) {
    let out = default_registry()
        .execute(
            "apply_patch",
            serde_json::json!({ "patch": patch }),
            ctx.clone(),
            CancellationToken::new(),
        )
        .await
        .expect("apply_patch dispatch");
    (out.is_error, out.content)
}

/// An exact hunk still applies. The strictness must not cost the tool its job.
#[tokio::test]
async fn an_exact_hunk_applies() {
    let (dir, ctx) = workspace("exact", "a.rs", "fn a() {}\nfn b() {}\n");
    let (is_error, content) = patch(
        &ctx,
        "*** Begin Patch\n*** Update File: a.rs\n fn a() {}\n-fn b() {}\n+fn b() { todo!() }\n*** End Patch\n",
    )
    .await;
    assert!(!is_error, "{content}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn a() {}\nfn b() { todo!() }\n"
    );
}

/// Typographic quotes on disk, ASCII quotes in the patch. The bytes differ, so
/// the hunk is not located. Accepting it would rewrite the curly quotes on a
/// context line the model never touched — which is exactly what `replace`
/// refuses to do for the identical input.
#[tokio::test]
async fn typographic_punctuation_is_not_folded_into_a_match() {
    let original = "let title = \u{201C}Deploy\u{201D};\nlet n = 1;\n";
    let (dir, ctx) = workspace("punct", "a.rs", original);
    let (is_error, content) = patch(
        &ctx,
        "*** Begin Patch\n*** Update File: a.rs\n let title = \"Deploy\";\n-let n = 1;\n+let n = 2;\n*** End Patch\n",
    )
    .await;
    assert!(is_error, "a near miss must fail, not be folded: {content}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        original,
        "a failed patch writes nothing"
    );
}

/// Spacing drift inside a line is a different line. Locating it and splicing
/// the model's spelling over the file's would silently reformat source the
/// model did not ask to change.
#[tokio::test]
async fn internal_spacing_drift_is_not_squashed_into_a_match() {
    let original = "fn t(x: u8) -> u8 {\n    x + 1\n}\n";
    let (dir, ctx) = workspace("squash", "a.rs", original);
    let (is_error, content) = patch(
        &ctx,
        // Space marker, then the model's spelling of the body line: four
        // spaces of indent but no spaces around the operator.
        "*** Begin Patch\n*** Update File: a.rs\n     x+1\n+    // note\n*** End Patch\n",
    )
    .await;
    assert!(is_error, "{content}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        original
    );
}

/// Indentation is program structure. A pattern whose leading whitespace does
/// not match must not locate the same statement at another nesting level.
#[tokio::test]
async fn indentation_is_not_trimmed_away_when_locating() {
    let original = "if a:\n    x = 1\nif b:\n        x = 1\n";
    let (dir, ctx) = workspace("indent", "a.py", original);
    // Written flush-left; the file has it at two different indents and neither
    // is column 0.
    let (is_error, content) = patch(
        &ctx,
        "*** Begin Patch\n*** Update File: a.py\n-x = 1\n+x = 2\n*** End Patch\n",
    )
    .await;
    assert!(
        is_error,
        "an unindented pattern must not match either line: {content}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.py")).unwrap(),
        original
    );
}

/// Trailing whitespace is bytes in the file. Dropping it silently on a context
/// line is still an edit the model did not request.
#[tokio::test]
async fn trailing_whitespace_on_a_context_line_is_not_silently_stripped() {
    let original = "fn main() {  \n    body();\n}\n";
    let (dir, ctx) = workspace("trailws", "a.rs", original);
    let (is_error, content) = patch(
        &ctx,
        "*** Begin Patch\n*** Update File: a.rs\n fn main() {\n-    body();\n+    body()?;\n*** End Patch\n",
    )
    .await;
    assert!(is_error, "{content}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        original,
        "the file's trailing whitespace must survive a refused patch"
    );
}

/// The property behind all of the above: when a patch does succeed, every line
/// it did not mark for change is byte-identical afterwards.
#[tokio::test]
async fn a_successful_patch_leaves_context_lines_byte_identical() {
    // Context lines carrying trailing whitespace and a non-breaking space,
    // copied verbatim into the patch.
    let original = "keep\u{00A0}me  \n-target-\nalso  keep\n";
    let (dir, ctx) = workspace("bytes", "a.txt", original);
    let (is_error, content) = patch(
        &ctx,
        "*** Begin Patch\n*** Update File: a.txt\n keep\u{00A0}me  \n--target-\n+-changed-\n also  keep\n*** End Patch\n",
    )
    .await;
    assert!(!is_error, "{content}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "keep\u{00A0}me  \n-changed-\nalso  keep\n",
        "only the '-' line may differ"
    );
}

/// A failure names what is actually in the file, which is the mechanical fact
/// the caller is missing. That is reporting the constraint, not a strategy.
#[tokio::test]
async fn a_failed_hunk_reports_what_the_file_really_contains() {
    let (_dir, ctx) = workspace("report", "a.rs", "fn main() {\n    let total = a + b;\n}\n");
    let (is_error, content) = patch(
        &ctx,
        "*** Begin Patch\n*** Update File: a.rs\n-    let total = a+b;\n+    let total = a.checked_add(b)?;\n*** End Patch\n",
    )
    .await;
    assert!(is_error, "{content}");
    assert!(
        content.contains("let total = a + b;"),
        "the error must show the real line: {content}"
    );
}

/// The description states the contract. It does not tell the caller what to do
/// with its own context budget afterwards.
#[test]
fn the_edit_description_states_the_contract_without_coaching() {
    let registry = default_registry();
    let edit = registry
        .definitions()
        .into_iter()
        .find(|d| d.name == "apply_patch")
        .expect("apply_patch registered");
    assert!(
        edit.description.contains("EXACTLY"),
        "the matching contract must be stated: {}",
        edit.description
    );
    for coaching in ["burns context", "do NOT re-read"] {
        assert!(
            !edit.description.contains(coaching),
            "tool descriptions state capability, not strategy ({coaching:?})"
        );
    }
}
