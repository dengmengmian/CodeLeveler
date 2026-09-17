//! P2 gate for the Core Primitive Foundation: `edit` and `write`.
//!
//! `apply_patch` is the canonical structured edit and `write_file` is the
//! canonical whole-file write. Both go through one guarded editor, so the
//! compare-and-swap, the write scope, the checkpoint and rollback are the same
//! guarantees for both — there is no second path to a file on disk.

use leveler_execution::{PermissionProfile, Workspace};
use leveler_tools::ToolContext;
use leveler_tools::default_registry;
use tokio_util::sync::CancellationToken;

fn workspace(name: &str) -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("leveler-p2-{name}-"))
        .tempdir()
        .unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let ctx = ToolContext::new(ws, PermissionProfile::Assisted);
    (dir, ctx)
}

async fn call(ctx: &ToolContext, tool: &str, args: serde_json::Value) -> (bool, String) {
    let out = default_registry()
        .execute(tool, args, ctx.clone(), CancellationToken::new())
        .await
        .unwrap_or_else(|e| panic!("{tool} failed: {e}"));
    (out.is_error, out.content)
}

/// Creating a file is a distinct model intent from editing part of one, and
/// the primitive that expresses it exists.
#[tokio::test]
async fn write_file_creates_a_new_file() {
    let (dir, ctx) = workspace("create");
    let (is_error, content) = call(
        &ctx,
        "write_file",
        serde_json::json!({"path": "src/new.rs", "content": "pub fn a() {}\n"}),
    )
    .await;
    assert!(!is_error, "{content}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/new.rs")).unwrap(),
        "pub fn a() {}\n"
    );
}

/// Replacing a whole file on purpose is the other half of the same intent.
#[tokio::test]
async fn write_file_replaces_a_file_it_has_read() {
    let (dir, ctx) = workspace("replace");
    std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
    let (is_error, _) = call(&ctx, "read_file", serde_json::json!({"path": "a.txt"})).await;
    assert!(!is_error);

    let (is_error, content) = call(
        &ctx,
        "write_file",
        serde_json::json!({"path": "a.txt", "content": "new\n"}),
    )
    .await;
    assert!(!is_error, "{content}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "new\n"
    );
}

/// The stale-write guarantee applies to `write_file` exactly as it does to
/// `apply_patch`: an overwrite built on an observation something else has
/// invalidated is refused, not resolved.
#[tokio::test]
async fn write_file_refuses_to_clobber_a_file_that_changed_since_it_was_read() {
    let (dir, ctx) = workspace("stale");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "read this\n").unwrap();
    let (is_error, _) = call(&ctx, "read_file", serde_json::json!({"path": "a.txt"})).await;
    assert!(!is_error);

    std::fs::write(&path, "someone else wrote this\n").unwrap();

    let (is_error, content) = call(
        &ctx,
        "write_file",
        serde_json::json!({"path": "a.txt", "content": "mine\n"}),
    )
    .await;
    assert!(
        is_error,
        "a stale whole-file write must be refused: {content}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "someone else wrote this\n",
        "the other writer's content must survive"
    );
}

/// The tool reports what it changed, or the turn finishes claiming nothing
/// was written.
#[tokio::test]
async fn write_file_reports_the_file_it_modified() {
    let (_dir, ctx) = workspace("evidence");
    let out = default_registry()
        .execute(
            "write_file",
            serde_json::json!({"path": "a.txt", "content": "x\n"}),
            ctx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.metadata["modified_files"],
        serde_json::json!(["a.txt"]),
        "{:?}",
        out.metadata
    );
}

/// Write scope is authority, not advice: it is enforced for the whole-file
/// primitive the same way it is for the patch primitive.
#[tokio::test]
async fn write_file_obeys_the_write_scope() {
    let (dir, ctx) = workspace("scope");
    let ctx = ctx.with_command_write_constraints(Some(Vec::new()), None, Vec::new());
    let (is_error, content) = call(
        &ctx,
        "write_file",
        serde_json::json!({"path": "a.txt", "content": "x\n"}),
    )
    .await;
    assert!(
        is_error,
        "an empty write scope must refuse the write: {content}"
    );
    assert!(!dir.path().join("a.txt").exists(), "nothing may be written");
}

/// A whole-file write of a path that leaves the workspace is refused before
/// anything is staged.
#[tokio::test]
async fn write_file_refuses_a_path_outside_the_workspace() {
    let (dir, ctx) = workspace("escape");
    let (is_error, content) = call(
        &ctx,
        "write_file",
        serde_json::json!({"path": "../escaped.txt", "content": "x\n"}),
    )
    .await;
    assert!(is_error, "{content}");
    assert!(
        !dir.path().parent().unwrap().join("escaped.txt").exists(),
        "nothing may be written outside the workspace"
    );
}
