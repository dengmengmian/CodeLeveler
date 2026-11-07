//! Behavior tests for the read-only foreign-session adapters.
//!
//! Fixtures reproduce the record shapes observed in real local Claude Code and
//! Codex stores. They are deliberately small and opinionated: every record
//! that matters to the digest has a counterpart that must be ignored.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use leveler_foreign_session::*;

const CLAUDE_ID: &str = "11111111-1111-4111-8111-111111111111";
const CODEX_ID: &str = "22222222-2222-4222-8222-222222222222";
const OTHER_ID: &str = "33333333-3333-4333-8333-333333333333";

fn temp_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

fn write_lines(path: &Path, lines: &[String]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, lines.join("\n") + "\n").expect("write fixture");
}

fn sanitize(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn claude_project(root: &Path, cwd: &Path, name_case: &str) -> PathBuf {
    let mut name = sanitize(cwd);
    if name_case == "lower" {
        name = name.to_ascii_lowercase();
    }
    root.join("projects").join(name)
}

fn claude_records(id: &str, cwd: &Path) -> Vec<String> {
    let cwd = cwd.display();
    vec![
        format!(
            r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd}","gitBranch":"main","timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"user","content":"Fix the failing build"}}}}"#
        ),
        format!(r#"{{"type":"ai-title","aiTitle":"Build repair","sessionId":"{id}"}}"#),
        format!(
            r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd}","isMeta":true,"message":{{"role":"user","content":"<local-command-caveat>not a request</local-command-caveat>"}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","sessionId":"{id}","cwd":"{cwd}","timestamp":"2026-01-01T00:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"I will inspect the test output."}}]}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{"command":"cargo test"}}}}]}}}}"#
        ),
        format!(
            r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"error: test failed"}}]}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","sessionId":"{id}","cwd":"{cwd}","isSidechain":true,"message":{{"role":"assistant","content":[{{"type":"text","text":"subagent note"}}]}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_2","name":"Edit","input":{{"file_path":"src/lib.rs"}}}}]}}}}"#
        ),
        format!(
            r#"{{"type":"system","sessionId":"{id}","subtype":"compact_boundary","content":"Conversation compacted","timestamp":"2026-01-01T00:00:03.000Z"}}"#
        ),
        format!(
            r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"user","content":"Also update the docs"}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","sessionId":"{id}","cwd":"{cwd}","timestamp":"2026-01-01T00:00:04.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"Done. All tests pass."}}]}}}}"#
        ),
        "{this is not json".to_string(),
    ]
}

fn claude_summary(root: &Path, cwd: &Path, state: &str) -> ForeignSessionSummary {
    let project = claude_project(root, cwd, state);
    let path = project.join(format!("{CLAUDE_ID}.jsonl"));
    write_lines(&path, &claude_records(CLAUDE_ID, cwd));
    find(ForeignTool::ClaudeCode, root, CLAUDE_ID).expect("find claude fixture")
}

fn codex_records(id: &str, cwd: &Path) -> Vec<String> {
    let cwd = cwd.display();
    vec![
        format!(
            r#"{{"type":"session_meta","payload":{{"id":"{id}","session_id":"{id}","cwd":"{cwd}","timestamp":"2026-01-01T00:00:00.000Z","source":"vscode"}}}}"#
        ),
        format!(
            r#"{{"type":"turn_context","payload":{{"cwd":"{cwd}","model":"gpt-6"}}}}"#
        ),
        format!(
            r##"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"# AGENTS.md instructions for {cwd}\n<INSTRUCTIONS>ignore me</INSTRUCTIONS>"}}]}}}}"##
        ),
        format!(
            r##"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"# Files pasted by the user:\n\nFix the build\n\n## My request:\n"}}]}}}}"##
        ),
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"I will run the tests."}}]}}}}"#
        ),
        r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"call_1","name":"exec","input":"git status"}}"#.to_string(),
        r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_1","output":"Exit code: 1\nfatal: not a git repository"}}"#.to_string(),
        r#"{"type":"response_item","payload":{"type":"function_call","call_id":"call_2","name":"shell","arguments":"{\"command\":[\"cargo\",\"test\"]}"}}"#.to_string(),
        r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_2","output":"ok"}}"#.to_string(),
        r#"{"type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"opaque"}}"#.to_string(),
        r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#.to_string(),
        r#"{"type":"compacted","payload":{"replacement_history":[]}}"#.to_string(),
        "{broken".to_string(),
    ]
}

fn codex_rollout(root: &Path, id: &str) -> PathBuf {
    root.join("sessions")
        .join("2026")
        .join("01")
        .join("02")
        .join(format!("rollout-2026-01-02T03-04-05-{id}.jsonl"))
}

fn codex_summary(root: &Path, cwd: &Path) -> ForeignSessionSummary {
    let path = codex_rollout(root, CODEX_ID);
    write_lines(&path, &codex_records(CODEX_ID, cwd));
    find(ForeignTool::Codex, root, CODEX_ID).expect("find codex fixture")
}

#[test]
fn claude_digest_separates_task_evidence_claims_and_incomplete_work() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let summary = claude_summary(root.path(), cwd, "exact");
    let digest = digest(&summary).expect("digest");

    assert_eq!(summary.title, "Build repair");
    assert_eq!(summary.cwd.as_deref(), Some(cwd));
    assert_eq!(summary.branch.as_deref(), Some("main"));
    assert!(summary.updated_at.ends_with('Z'));

    assert_eq!(
        digest.first_user_message.as_deref(),
        Some("Fix the failing build")
    );
    assert_eq!(
        digest.early_user_messages,
        vec!["Fix the failing build", "Also update the docs"]
    );
    // A model's completion sentence is preserved as a claim, and the failed
    // tool result remains the evidence next to it.
    assert_eq!(
        digest.last_assistant_message.as_deref(),
        Some("Done. All tests pass.")
    );
    assert_eq!(digest.last_error.as_deref(), Some("error: test failed"));
    assert_eq!(digest.stats.compactions, 1);
    assert_eq!(digest.stats.sidechain_events, 1);
    assert_eq!(digest.stats.malformed_lines, 1);
    assert_eq!(digest.stats.user_messages, 2);
    assert_eq!(digest.stats.tool_calls, 2);
    assert_eq!(digest.stats.tool_results, 1);
    assert_eq!(digest.stats.assistant_messages, 3);
    assert_eq!(
        digest
            .open_tool_calls
            .iter()
            .map(|call| call.call_id.as_str())
            .collect::<Vec<_>>(),
        vec!["toolu_2"]
    );
    assert!(
        digest
            .recent
            .iter()
            .any(|event| event.kind == ForeignEventKind::Assistant)
    );
}

#[test]
fn claude_find_accepts_both_observed_project_dir_casings() {
    let root = temp_root();
    let cwd = Path::new("/Users/Someone/Develop/CodeLeveler");

    for state in ["exact", "lower"] {
        let dir = claude_project(root.path(), cwd, state);
        write_lines(
            &dir.join(format!("{OTHER_ID}.jsonl")),
            &claude_records(OTHER_ID, cwd),
        );
        let found = find(ForeignTool::ClaudeCode, root.path(), OTHER_ID).expect(state);
        assert_eq!(found.native_id, OTHER_ID);
        fs::remove_file(dir.join(format!("{OTHER_ID}.jsonl"))).expect("cleanup");
    }
}

#[test]
fn discovery_matches_cwd_sorts_newest_and_respects_the_limit() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let other = Path::new("/tmp/work/other");
    let dir = claude_project(root.path(), cwd, "exact");
    fs::create_dir_all(&dir).expect("project dir");
    let other_dir = claude_project(root.path(), other, "exact");
    write_lines(
        &other_dir.join(format!("{OTHER_ID}.jsonl")),
        &claude_records(OTHER_ID, other),
    );

    for (_offset, id) in [
        (1, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
        (2, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"),
    ] {
        let path = dir.join(format!("{id}.jsonl"));
        write_lines(&path, &claude_records(id, cwd));
        std::thread::sleep(Duration::from_millis(2));
    }

    let first = discover(
        ForeignTool::ClaudeCode,
        root.path(),
        Some(cwd),
        10,
        SystemTime::now(),
    )
    .expect("discover");
    assert_eq!(first.len(), 2, "only the requested project");
    assert_eq!(first[0].native_id, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
    assert!(
        first
            .iter()
            .all(|summary| summary.cwd.as_deref() == Some(cwd)),
        "foreign project must not leak in"
    );

    let limited = discover(
        ForeignTool::ClaudeCode,
        root.path(),
        Some(cwd),
        1,
        SystemTime::now(),
    )
    .expect("discover");
    assert_eq!(limited.len(), 1);
}

#[test]
fn discovery_titles_skip_injected_commands_and_continuation_preambles() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let dir = claude_project(root.path(), cwd, "exact");
    fs::create_dir_all(&dir).expect("project dir");
    let id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let cwd_text = cwd.display();
    write_lines(
        &dir.join(format!("{id}.jsonl")),
        &[
            format!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd_text}","message":{{"role":"user","content":"This session is being continued from a previous conversation that ran out of context."}}}}"#
            ),
            format!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd_text}","message":{{"role":"user","content":"<command-name>/clear</command-name> <command-message>clear</command-message> <command-args></command-args>"}}}}"#
            ),
            format!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd_text}","message":{{"role":"user","content":"修复登录失败的问题"}}}}"#
            ),
        ],
    );
    let found = discover(
        ForeignTool::ClaudeCode,
        root.path(),
        Some(cwd),
        10,
        SystemTime::now(),
    )
    .expect("discover");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].title, "修复登录失败的问题");
}

#[test]
fn discovery_excludes_sessions_older_than_the_age_bound() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let _ = claude_summary(root.path(), cwd, "exact");
    let future = SystemTime::now() + Duration::from_secs((MAX_SESSION_AGE_DAYS + 1) * 86_400);
    let found =
        discover(ForeignTool::ClaudeCode, root.path(), Some(cwd), 10, future).expect("discover");
    assert!(found.is_empty());
}

#[test]
fn codex_digest_skips_injected_instructions_and_counts_abort_and_compaction() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let summary = codex_summary(root.path(), cwd);
    assert_eq!(summary.native_id, CODEX_ID);
    assert_eq!(summary.cwd.as_deref(), Some(cwd));

    let digest = digest(&summary).expect("digest");
    let first = digest.first_user_message.as_deref().expect("task");
    assert!(first.starts_with("# Files pasted by the user:"), "{first}");
    assert!(!first.contains("AGENTS.md"));
    assert_eq!(digest.stats.assistant_messages, 1);
    assert_eq!(digest.stats.tool_calls, 2);
    assert_eq!(digest.stats.tool_results, 2);
    assert_eq!(digest.stats.errors, 1);
    assert_eq!(digest.stats.compactions, 1);
    assert_eq!(digest.stats.malformed_lines, 1);
    assert!(digest.turn_aborted);
    assert!(digest.open_tool_calls.is_empty());
    assert!(
        digest
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("fatal: not a git repository"))
    );
}

#[test]
fn codex_find_walks_date_directories() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let summary = codex_summary(root.path(), cwd);
    assert_eq!(
        summary.title,
        "# Files pasted by the user: Fix the build ## My request:"
    );
    let file_name = summary
        .source_path
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .to_string();
    assert!(
        file_name.ends_with(&format!("-{CODEX_ID}.jsonl")),
        "{file_name}"
    );
}

#[test]
fn index_previews_are_bounded_and_ordered() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let summary = claude_summary(root.path(), cwd, "exact");
    let index = index(&summary).expect("index");
    assert!(!index.truncated);
    assert!(index.events.len() >= 8);
    assert!(
        index
            .events
            .windows(2)
            .all(|pair| pair[0].index + 1 == pair[1].index)
    );
    assert!(index.events.iter().all(|event| event.preview.len() <= 241));
    assert!(
        index
            .events
            .iter()
            .any(|event| event.kind == ForeignEventKind::ToolResult && !event.sidechain)
    );
    assert!(
        index
            .events
            .iter()
            .any(|event| event.kind == ForeignEventKind::Assistant && event.sidechain)
    );
}

#[test]
fn read_events_returns_full_text_and_respects_the_limit() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let summary = claude_summary(root.path(), cwd, "exact");
    let index = index(&summary).expect("index");
    let result_event = index
        .events
        .iter()
        .find(|event| event.kind == ForeignEventKind::ToolResult)
        .expect("tool result");
    let details = read_events(&summary, result_event.index, 1).expect("read");
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].kind, ForeignEventKind::ToolResult);
    assert!(details[0].is_error);
    assert!(details[0].text.contains("error: test failed"));

    let first = read_events(&summary, 1, 100).expect("read");
    assert_eq!(first.len(), index.events.len().min(50));
}

#[test]
fn search_is_case_insensitive_and_bounded() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let summary = claude_summary(root.path(), cwd, "exact");
    let hits = search(&summary, "TESTS PASS", 10).expect("search");
    assert!(!hits.is_empty());
    assert!(hits[0].snippet.to_lowercase().contains("tests pass"));
    let bounded = search(&summary, "e", 1).expect("search");
    assert_eq!(bounded.len(), 1);
}

#[test]
fn reads_never_modify_the_source_store() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let summary = claude_summary(root.path(), cwd, "exact");
    let before_bytes = fs::read(&summary.source_path).expect("read before");
    let before_modified = fs::metadata(&summary.source_path)
        .expect("metadata before")
        .modified()
        .expect("mtime");
    let before_files = list_files(root.path());

    let _ = discover(
        ForeignTool::ClaudeCode,
        root.path(),
        Some(cwd),
        10,
        SystemTime::now(),
    )
    .expect("discover");
    let _ = digest(&summary).expect("digest");
    let _ = index(&summary).expect("index");
    let _ = search(&summary, "build", 5).expect("search");
    let _ = read_events(&summary, 1, 2).expect("read");

    assert_eq!(
        fs::read(&summary.source_path).expect("read after"),
        before_bytes
    );
    assert_eq!(
        fs::metadata(&summary.source_path)
            .expect("metadata after")
            .modified()
            .expect("mtime"),
        before_modified
    );
    assert_eq!(list_files(root.path()), before_files);
}

fn list_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
fn invalid_ids_are_refused_before_touching_the_filesystem() {
    let root = temp_root();
    for id in ["../../etc/passwd", "a/b", "", "not an id"] {
        let error = find(ForeignTool::Codex, root.path(), id).expect_err("reject");
        assert!(
            matches!(error, ForeignSessionError::InvalidId { .. }),
            "{id}: {error:?}"
        );
    }
}

#[test]
fn missing_corrupt_and_incompatible_sessions_have_distinct_errors() {
    let root = temp_root();
    let missing = find(ForeignTool::Codex, root.path(), OTHER_ID).expect_err("missing");
    assert!(matches!(missing, ForeignSessionError::NotFound { .. }));

    let path = codex_rollout(root.path(), CODEX_ID);
    fs::create_dir_all(path.parent().unwrap()).expect("dirs");
    fs::write(&path, "not json\nstill not json\n").expect("corrupt");
    let corrupt = find(ForeignTool::Codex, root.path(), CODEX_ID).expect_err("corrupt");
    assert!(matches!(corrupt, ForeignSessionError::Corrupt { .. }));

    fs::write(&path, "{\"type\":\"future_record\",\"payload\":{}}\n").expect("incompatible");
    let incompatible = find(ForeignTool::Codex, root.path(), CODEX_ID).expect_err("incompatible");
    assert!(matches!(
        incompatible,
        ForeignSessionError::Incompatible { .. }
    ));
}

#[test]
fn compressed_only_rollout_is_reported_as_unsupported() {
    let root = temp_root();
    let path = codex_rollout(root.path(), CODEX_ID);
    fs::create_dir_all(path.parent().unwrap()).expect("dirs");
    fs::write(path.with_extension("jsonl.zst"), b"compressed").expect("zst");
    let error = find(ForeignTool::Codex, root.path(), CODEX_ID).expect_err("zst");
    assert!(matches!(
        error,
        ForeignSessionError::UnsupportedFormat { .. }
    ));
}

#[test]
fn a_corrupt_line_does_not_hide_the_rest_of_a_session() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let summary = claude_summary(root.path(), cwd, "exact");
    let digest = digest(&summary).expect("digest");
    assert_eq!(digest.stats.malformed_lines, 1);
    assert_eq!(
        digest.last_assistant_message.as_deref(),
        Some("Done. All tests pass.")
    );
}

#[test]
fn digested_text_is_bounded_for_a_huge_assistant_message() {
    let root = temp_root();
    let cwd = Path::new("/tmp/work/proj");
    let dir = claude_project(root.path(), cwd, "exact");
    let huge = "x".repeat(200_000);
    let lines = vec![
        format!(
            r#"{{"type":"user","sessionId":"{CLAUDE_ID}","cwd":"{}","message":{{"role":"user","content":"task"}}}}"#,
            cwd.display()
        ),
        format!(
            r#"{{"type":"assistant","sessionId":"{CLAUDE_ID}","cwd":"{}","message":{{"role":"assistant","content":[{{"type":"text","text":"{huge}"}}]}}}}"#,
            cwd.display()
        ),
    ];
    write_lines(&dir.join(format!("{CLAUDE_ID}.jsonl")), &lines);
    let summary = find(ForeignTool::ClaudeCode, root.path(), CLAUDE_ID).expect("find");
    let digest = digest(&summary).expect("digest");
    assert!(digest.last_assistant_message.as_ref().unwrap().len() <= 8 * 1024);
    let index = index(&summary).expect("index");
    assert!(index.events.iter().all(|event| event.preview.len() <= 241));
    let details = read_events(&summary, 2, 1).expect("read");
    assert!(details[0].truncated);
    assert!(details[0].text.len() <= MAX_EVENT_TEXT_BYTES);
}
