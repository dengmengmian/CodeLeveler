//! Stage 1 Git isolation, path safety, and secret rejection.

mod memory_v2_common;

use std::fs;
use std::path::Path;
use std::process::Command;

use leveler_memory::v2::{
    DocumentError, MarkdownStore, MemoryScope, PathError, ScopeRoots, StoreError,
};
use memory_v2_common::*;

fn git_ignored(repo: &Path, path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("check-ignore")
        .arg("-q")
        .arg(path)
        .status()
        .expect("git must be installed for local-scope tests")
        .success()
}

#[test]
fn local_memory_files_are_ignored_without_touching_the_repository_gitignore() {
    let fixture = Fixture::new(MemoryScope::Local);
    let repo = fixture.repo.as_ref().unwrap();
    let store = fixture.store();
    let created = create_user(
        &store,
        MemoryScope::Local,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    );
    let id = applied_id(&created);
    let file = fixture.current_file(&id);
    assert!(file.starts_with(repo));
    assert!(
        git_ignored(repo, &file),
        "local memory file must be ignored"
    );
    assert!(
        git_ignored(repo, &fixture.current.join(".stage").join("probe")),
        "the transaction staging directory must be ignored"
    );
    assert!(
        !repo.join(".gitignore").exists(),
        "the repository root .gitignore must not be created or modified"
    );
    let ignore = fs::read_to_string(fixture.current.join(".gitignore")).unwrap();
    assert_eq!(ignore.lines().collect::<Vec<_>>(), vec!["*", "!.gitignore"]);
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("ls-files")
        .arg("--others")
        .arg("--exclude-standard")
        .output()
        .unwrap();
    let untracked = String::from_utf8_lossy(&status.stdout);
    assert!(
        !untracked.lines().any(|line| line.ends_with(".md")),
        "no local memory document may be visible to Git: {untracked}"
    );
}

#[test]
fn reopening_a_local_store_keeps_the_git_sentinel_idempotent() {
    let fixture = Fixture::new(MemoryScope::Local);
    let first = fixture.store();
    let id = applied_id(&create_user(
        &first,
        MemoryScope::Local,
        "billing_model",
        "subscription",
        "The provider bills by subscription.",
    ));
    drop(first);
    let second = MarkdownStore::open(fixture.roots()).unwrap();
    assert!(second.read(&id).is_ok());
    let ignore = fs::read_to_string(fixture.current.join(".gitignore")).unwrap();
    assert_eq!(ignore.lines().collect::<Vec<_>>(), vec!["*", "!.gitignore"]);
}

#[test]
fn project_scope_ignores_only_transaction_temporaries() {
    let fixture = Fixture::new(MemoryScope::Project);
    let _store = fixture.store();
    let ignore = fs::read_to_string(fixture.current.join(".gitignore")).unwrap();
    let lines: Vec<&str> = ignore.lines().collect();
    assert_eq!(lines, vec![".stage/", ".tmp-*"]);
}

#[test]
fn a_state_root_inside_the_repository_is_rejected() {
    let fixture = Fixture::new(MemoryScope::Local);
    let repo = fixture.repo.as_ref().unwrap();
    let roots = ScopeRoots::new(
        MemoryScope::Local,
        repo.join(".leveler").join("local"),
        repo.join(".leveler").join("state"),
    )
    .with_repo_root(repo);
    let error = MarkdownStore::open(roots).err().unwrap();
    assert!(
        matches!(error, StoreError::Path(PathError::StateInsideRepo)),
        "{error:?}"
    );
}

#[test]
fn the_repository_root_itself_cannot_be_the_local_memory_root() {
    let fixture = Fixture::new(MemoryScope::Local);
    let repo = fixture.repo.as_ref().unwrap();
    let roots =
        ScopeRoots::new(MemoryScope::Local, repo, fixture.dir().join("state")).with_repo_root(repo);
    let error = MarkdownStore::open(roots).err().unwrap();
    assert!(
        matches!(error, StoreError::Path(PathError::ScopeUnavailable(_))),
        "{error:?}"
    );
}

#[test]
fn a_non_empty_directory_is_not_silently_ignored() {
    let fixture = Fixture::new(MemoryScope::Project);
    fs::create_dir_all(&fixture.current).unwrap();
    fs::write(fixture.current.join("unrelated.txt"), b"keep me").unwrap();
    let error = MarkdownStore::open(fixture.roots()).err().unwrap();
    assert!(
        matches!(error, StoreError::Path(PathError::GitIgnoreConflict(_))),
        "{error:?}"
    );
    assert!(!fixture.current.join(".gitignore").exists());
    assert_eq!(
        fs::read_to_string(fixture.current.join("unrelated.txt")).unwrap(),
        "keep me"
    );
}

#[test]
fn a_credential_in_the_body_is_rejected_before_any_file_is_written() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let error = store
        .admit(leveler_memory::v2::AdmitRequest {
            op_id: op(),
            caller: user(),
            intent: fact_intent(
                MemoryScope::Project,
                Some("provider_key"),
                "set",
                "api_key=sk-live-1234567890",
            ),
            expected: None,
        })
        .unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::Document(DocumentError::SecretDetected("body"))
        ),
        "{error:?}"
    );
    assert!(effective(&store).is_empty());
    assert!(store.list_pending().unwrap().is_empty());
    let documents: Vec<_> = fs::read_dir(&fixture.current)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".md"))
        .collect();
    assert!(
        documents.is_empty(),
        "no document may be written: {documents:?}"
    );
}

#[test]
fn a_credential_in_the_value_is_rejected() {
    let fixture = Fixture::new(MemoryScope::Project);
    let store = fixture.store();
    let error = store
        .admit(leveler_memory::v2::AdmitRequest {
            op_id: op(),
            caller: user(),
            intent: fact_intent(
                MemoryScope::Project,
                Some("provider_key"),
                "client_secret=abc",
                "The provider key is stored outside this record.",
            ),
            expected: None,
        })
        .unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::Document(DocumentError::SecretDetected("value"))
        ),
        "{error:?}"
    );
}
