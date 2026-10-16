//! Deterministic gate for the code-intelligence tools: `find_symbol`,
//! `read_symbol`, `find_references` and `diagnostics`.
//!
//! Every one of these tools has two paths: a language server when one is
//! installed, and a documented fallback that answers a weaker question and says
//! so. The live-server path is a timing test that only runs on a machine with a
//! server (see `find_symbol_lsp.rs`); this file locks the half that must hold
//! everywhere — the tool answers about the workspace it was given, and where it
//! cannot answer it reports the limitation instead of inventing a result.
//!
//! Assertions are content-based on purpose: whether the answer came from a
//! server or from the scan, the defining file must be the one named, and
//! `diagnostics` on a language with no wired server must say so rather than
//! return a fabricated clean bill.

use leveler_execution::{PermissionProfile, Workspace};
use leveler_tools::ToolContext;
use leveler_tools::default_registry;
use tokio_util::sync::CancellationToken;

fn workspace(name: &str) -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("leveler-ci-{name}-"))
        .tempdir()
        .unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let ctx = ToolContext::new(ws, PermissionProfile::RequestApproval);
    (dir, ctx)
}

async fn call(ctx: &ToolContext, tool: &str, args: serde_json::Value) -> (bool, String) {
    let out = default_registry()
        .execute(tool, args, ctx.clone(), CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{tool} failed: {e}"));
    (out.is_error, out.content)
}

/// A definition site, a separate caller, and no language server wired for the
/// extension the tests use for `diagnostics`.
fn seed(dir: &tempfile::TempDir) {
    std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
    std::fs::write(
        dir.path().join("pkg/math.py"),
        "def triple(x):\n    return x * 3\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("pkg/caller.py"),
        "from pkg.math import triple\n\n\ndef run():\n    return triple(2)\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("pkg/notes.xyz"),
        "triple is mentioned here\n",
    )
    .unwrap();
}

/// `find_symbol` names the file that DEFINES the symbol, not every file that
/// mentions it.
#[tokio::test]
async fn find_symbol_names_the_defining_file() {
    let (dir, ctx) = workspace("find-symbol");
    seed(&dir);
    let (is_error, content) =
        call(&ctx, "find_symbol", serde_json::json!({"symbol": "triple"})).await;
    assert!(!is_error, "{content}");
    assert!(
        content.contains("pkg/math.py"),
        "the definition site must be named: {content}"
    );
    assert!(
        !content.contains("pkg/caller.py"),
        "a call site is not a definition: {content}"
    );
}

/// `read_symbol` returns the definition body without loading the whole file.
#[tokio::test]
async fn read_symbol_returns_the_definition_body() {
    let (dir, ctx) = workspace("read-symbol");
    seed(&dir);
    let (is_error, content) =
        call(&ctx, "read_symbol", serde_json::json!({"symbol": "triple"})).await;
    assert!(!is_error, "{content}");
    assert!(
        content.contains("return x * 3"),
        "the body must be returned: {content}"
    );
    assert!(
        content.contains("pkg/math.py"),
        "the file must be named: {content}"
    );
}

/// `find_references` reports the call site; a definition alone is not a
/// reference answer.
#[tokio::test]
async fn find_references_reports_the_call_site() {
    let (dir, ctx) = workspace("find-references");
    seed(&dir);
    let (is_error, content) = call(
        &ctx,
        "find_references",
        serde_json::json!({"symbol": "triple"}),
    )
    .await;
    assert!(!is_error, "{content}");
    assert!(
        content.contains("pkg/caller.py"),
        "the call site must be reported: {content}"
    );
}

/// An unknown symbol is reported as not found, not invented.
#[tokio::test]
async fn find_symbol_says_when_nothing_defines_the_name() {
    let (dir, ctx) = workspace("find-symbol-miss");
    seed(&dir);
    let (is_error, content) = call(
        &ctx,
        "find_symbol",
        serde_json::json!({"symbol": "no_such_symbol_anywhere"}),
    )
    .await;
    assert!(!is_error, "{content}");
    assert!(
        content.contains("no definition"),
        "absence must be reported: {content}"
    );
}

/// `diagnostics` on a path whose language is not recognized answers honestly
/// instead of returning an empty (fabricated clean) result.
#[tokio::test]
async fn diagnostics_reports_an_unrecognized_language() {
    let (dir, ctx) = workspace("diagnostics-language");
    seed(&dir);
    let (is_error, content) = call(
        &ctx,
        "diagnostics",
        serde_json::json!({"path": "pkg/notes.xyz"}),
    )
    .await;
    assert!(!is_error, "{content}");
    assert!(
        content.contains("no recognized language"),
        "an unknown language must be named: {content}"
    );
    assert!(
        !content.contains("0 error(s)"),
        "an unavailable answer is not a clean bill: {content}"
    );
}

/// `diagnostics` on a language with no wired server says the server is
/// unavailable; it never claims the file is clean.
#[tokio::test]
async fn diagnostics_names_a_missing_language_server() {
    let (dir, ctx) = workspace("diagnostics-server");
    std::fs::create_dir_all(dir.path().join("native")).unwrap();
    std::fs::write(
        dir.path().join("native/main.cpp"),
        "int main() { return 0; }\n",
    )
    .unwrap();
    let (is_error, content) = call(
        &ctx,
        "diagnostics",
        serde_json::json!({"path": "native/main.cpp"}),
    )
    .await;
    assert!(!is_error, "{content}");
    assert!(
        content.contains("no language server"),
        "the missing server must be reported: {content}"
    );
    assert!(
        !content.contains("0 error(s)"),
        "no server is not a clean bill: {content}"
    );
}
