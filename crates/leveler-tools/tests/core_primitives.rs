//! P1 gate for the Core Primitive Foundation: `read`, `ls`, `find`, `grep`.
//!
//! These lock the contracts `docs/ARCHITECTURE.md` §6.3 states for the four
//! read-side primitives. Every assertion here is mechanical: bounded output,
//! deterministic semantics that do not change with the machine, honest UTF-8,
//! and a stale-write guarantee that survives the refactor.

use leveler_execution::{PermissionProfile, Workspace};
use leveler_tools::ToolContext;
use leveler_tools::default_registry;
use tokio_util::sync::CancellationToken;

fn workspace(name: &str) -> (tempfile::TempDir, ToolContext) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("leveler-p1-{name}-"))
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

// ---------------------------------------------------------------- READ

/// A 100 MB file must still support a bounded window read. Refusing on size
/// and pointing the model at `sed`/`head`/`tail` is an incomplete primitive,
/// not a limit (§1.1, §18.3 A).
#[tokio::test]
async fn read_serves_a_bounded_window_of_a_very_large_file() {
    let (dir, ctx) = workspace("read-large");
    let path = dir.path().join("huge.txt");
    let line = "x".repeat(99);
    let mut body = String::with_capacity(12 * 1024 * 1024);
    for i in 0..120_000 {
        body.push_str(&format!("{i:06}{line}\n"));
    }
    assert!(
        body.len() > 10 * 1024 * 1024,
        "fixture must exceed the old cap"
    );
    std::fs::write(&path, &body).unwrap();

    let (is_error, content) = call(
        &ctx,
        "read_file",
        serde_json::json!({"path": "huge.txt", "start_line": 119_990, "end_line": 119_995}),
    )
    .await;
    assert!(
        !is_error,
        "a bounded window of a large file must succeed: {content}"
    );
    assert!(
        content.contains("119989"),
        "window content missing: {content}"
    );
    assert!(
        !content.contains("sed") && !content.contains("head") && !content.contains("tail"),
        "read must not push the model to a shell to page a large file: {content}"
    );
}

/// Invalid UTF-8 is reported, never silently rewritten into U+FFFD text that
/// the model would then treat as the file's contents (§1.1, §18.3 A).
#[tokio::test]
async fn read_reports_invalid_utf8_instead_of_inventing_text() {
    let (dir, ctx) = workspace("read-utf8");
    // Valid ASCII first line, then a lone continuation byte: no NUL anywhere,
    // so the old 8 KB NUL scan let this through the lossy path.
    std::fs::write(dir.path().join("bad.txt"), b"ok\nbad: \xC3\x28 tail\n").unwrap();

    let (is_error, content) = call(&ctx, "read_file", serde_json::json!({"path": "bad.txt"})).await;
    assert!(
        is_error,
        "invalid UTF-8 must be an explicit error: {content}"
    );
    assert!(
        content.to_lowercase().contains("utf-8"),
        "the error must name the constraint that was violated: {content}"
    );
    assert!(
        !content.contains('\u{FFFD}'),
        "replacement characters must not be presented as file truth: {content}"
    );
}

/// Reading the same unchanged range repeatedly is the model's business. The
/// filesystem reader does not annotate it (§1.1, §25).
#[tokio::test]
async fn read_does_not_nudge_the_model_about_repeated_reads() {
    let (dir, ctx) = workspace("read-repeat");
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    for _ in 0..6 {
        let (is_error, content) =
            call(&ctx, "read_file", serde_json::json!({"path": "a.txt"})).await;
        assert!(!is_error, "{content}");
        assert!(
            !content.contains("read multiple times"),
            "read must not editorialize about how often the model reads: {content}"
        );
    }
}

/// The stale-write guarantee is unchanged: a read observes a version, and an
/// edit built on that observation is refused once something else rewrites the
/// file underneath it.
#[tokio::test]
async fn read_still_observes_the_version_that_guards_a_later_edit() {
    let (dir, ctx) = workspace("read-stale");
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "alpha\n").unwrap();

    let (is_error, _) = call(&ctx, "read_file", serde_json::json!({"path": "a.txt"})).await;
    assert!(!is_error);

    std::fs::write(&path, "rewritten by someone else\n").unwrap();

    let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-alpha\n+beta\n*** End Patch\n";
    let (is_error, content) = call(&ctx, "apply_patch", serde_json::json!({"patch": patch})).await;
    assert!(
        is_error,
        "a patch built on a stale observation must be refused: {content}"
    );
}

// ------------------------------------------------------------------ LS

/// `ls` inspects one directory. It does not walk the tree (§26).
#[tokio::test]
async fn ls_lists_direct_children_only() {
    let (dir, ctx) = workspace("ls-direct");
    std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    std::fs::write(dir.path().join("src/deep/buried.rs"), "").unwrap();
    std::fs::write(dir.path().join("top.rs"), "").unwrap();
    std::fs::write(dir.path().join(".hidden"), "").unwrap();

    let (is_error, content) = call(&ctx, "list_files", serde_json::json!({})).await;
    assert!(!is_error, "{content}");
    assert!(content.contains("top.rs"), "{content}");
    assert!(
        content.contains("src/"),
        "directories are marked: {content}"
    );
    assert!(
        content.contains(".hidden"),
        "dotfiles are listed: {content}"
    );
    assert!(
        !content.contains("buried.rs"),
        "ls must not descend into subdirectories: {content}"
    );
}

// ---------------------------------------------------------------- FIND

/// One meaning for one invocation: the pattern is a glob. A bare word is a
/// name glob, not a forgiving substring (§27).
#[tokio::test]
async fn find_has_one_glob_semantics() {
    let (dir, ctx) = workspace("find-glob");
    std::fs::create_dir_all(dir.path().join("src/inner")).unwrap();
    std::fs::write(dir.path().join("src/inner/parser.rs"), "").unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();

    let (is_error, content) = call(
        &ctx,
        "find_files",
        serde_json::json!({"pattern": "**/*.rs"}),
    )
    .await;
    assert!(!is_error, "{content}");
    assert!(content.contains("src/inner/parser.rs"), "{content}");
    assert!(content.contains("src/lib.rs"), "{content}");

    // A bare word is a glob over the file name, so it does not match by
    // substring the way the old `auto` mode did.
    let (_, content) = call(&ctx, "find_files", serde_json::json!({"pattern": "pars"})).await;
    assert!(
        !content.contains("parser.rs"),
        "`pars` is a glob, not a substring: {content}"
    );

    let (_, content) = call(
        &ctx,
        "find_files",
        serde_json::json!({"pattern": "parser.rs"}),
    )
    .await;
    assert!(
        content.contains("src/inner/parser.rs"),
        "a bare name glob matches the file name at any depth: {content}"
    );
}

/// The same invocation means the same thing whether or not this directory is
/// a Git repository (§28).
#[tokio::test]
async fn find_does_not_change_meaning_with_git() {
    let (plain_dir, plain) = workspace("find-plain");
    let (git_dir, git) = workspace("find-git");
    for dir in [plain_dir.path(), git_dir.path()] {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("src/b.rs"), "").unwrap();
    }
    // A real Git repository, with an index that does not know the files.
    std::fs::create_dir_all(git_dir.path().join(".git")).unwrap();
    std::fs::write(git_dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let args = serde_json::json!({"pattern": "**/*.rs"});
    let (_, plain_out) = call(&plain, "find_files", args.clone()).await;
    let (_, git_out) = call(&git, "find_files", args).await;
    assert_eq!(
        plain_out, git_out,
        "find results must not depend on whether Git is present"
    );
}

/// The stronger form of the same guarantee: `find` and `grep` run entirely
/// in process. A tool that shells out to `git ls-files` or `rg` cannot claim
/// replay side-effect freedom, so this assertion is mechanical proof that
/// neither binary is in the path any more (§28, §30, §31).
#[test]
fn find_and_grep_are_pure_in_process_reads() {
    let registry = default_registry();
    for tool in ["find_files", "grep", "read_file", "list_files"] {
        assert!(
            registry.replay_is_side_effect_free(tool),
            "{tool} must be a pure in-process read"
        );
    }
}

// ---------------------------------------------------------------- GREP

/// The pattern is a regex, always. No machine turns it into a substring scan
/// behind the model's back (§30).
#[tokio::test]
async fn grep_pattern_is_always_a_regex() {
    let (dir, ctx) = workspace("grep-regex");
    std::fs::write(dir.path().join("a.rs"), "fn   main() {}\nfn other() {}\n").unwrap();

    let (is_error, content) = call(&ctx, "grep", serde_json::json!({"pattern": "fn +main"})).await;
    assert!(!is_error, "{content}");
    assert!(content.contains("a.rs:1:"), "regex must match: {content}");
    assert!(
        !content.contains("literal substring"),
        "no degraded-semantics note may appear: {content}"
    );
}

/// Literal search is asked for, not inferred from what happens to be installed.
#[tokio::test]
async fn grep_literal_mode_is_explicit() {
    let (dir, ctx) = workspace("grep-literal");
    std::fs::write(dir.path().join("a.rs"), "let x = a.b();\nlet y = axb();\n").unwrap();

    let (_, regex_out) = call(&ctx, "grep", serde_json::json!({"pattern": "a.b"})).await;
    assert!(
        regex_out.contains("axb()"),
        "`.` is a regex wildcard: {regex_out}"
    );

    let (_, literal_out) = call(
        &ctx,
        "grep",
        serde_json::json!({"pattern": "a.b", "literal": true}),
    )
    .await;
    assert!(literal_out.contains("a.b()"), "{literal_out}");
    assert!(
        !literal_out.contains("axb()"),
        "literal mode must not match the wildcard: {literal_out}"
    );
}

/// Case folding is a parameter, not a guess.
#[tokio::test]
async fn grep_ignore_case_is_a_parameter() {
    let (dir, ctx) = workspace("grep-case");
    std::fs::write(dir.path().join("a.rs"), "Needle\n").unwrap();

    let (_, sensitive) = call(&ctx, "grep", serde_json::json!({"pattern": "needle"})).await;
    assert!(sensitive.contains("(no matches)"), "{sensitive}");

    let (_, folded) = call(
        &ctx,
        "grep",
        serde_json::json!({"pattern": "needle", "ignore_case": true}),
    )
    .await;
    assert!(folded.contains("a.rs:1:Needle"), "{folded}");
}

/// An invalid regex is a precise error, not an empty result set.
#[tokio::test]
async fn grep_invalid_regex_is_reported() {
    let (dir, ctx) = workspace("grep-badregex");
    std::fs::write(dir.path().join("a.rs"), "x\n").unwrap();

    let (is_error, content) = call(&ctx, "grep", serde_json::json!({"pattern": "a(b"})).await;
    assert!(
        is_error,
        "an unparseable pattern must be an error: {content}"
    );
    assert!(
        content.to_lowercase().contains("regex"),
        "the error must name the constraint: {content}"
    );
}
