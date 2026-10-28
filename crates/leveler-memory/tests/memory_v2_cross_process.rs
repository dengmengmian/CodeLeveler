//! Cross-process reader/writer consistency for the Markdown store.
//!
//! The in-process concurrency tests share one address space, so a reader there
//! sees whatever a writer's own locks say. These tests use separate processes:
//! what the reader observes is only what the filesystem and the scope lock
//! expose, which is the contract a second CodeLeveler process actually gets.

mod memory_v2_common;

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use leveler_memory::v2::{
    Admission, Caller, Cas, MarkdownStore, MemoryId, MemoryScope, OpId, RecordPatch, ScopeRoots,
    StoreError,
};
use memory_v2_common::*;

const USER_INVOCATION: &str = "invocation-cross-process";
const WRITER_CALLER: &str = "writer";
const STALE_CALLER: &str = "stale";

fn user() -> Caller {
    Caller::UserCommand {
        invocation: USER_INVOCATION.to_string(),
    }
}

fn spawn(fixture: &Fixture, helper: &str, env: &[(&str, String)]) -> Child {
    let executable = std::env::current_exe().unwrap();
    let mut command = Command::new(executable);
    command
        .args(["--exact", helper, "--ignored", "--nocapture"])
        .env("V2_X_CURRENT", fixture.current.to_str().unwrap())
        .env("V2_X_STATE", fixture.state.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    command.spawn().expect("spawn the helper process")
}

fn wait_ok(child: Child, label: &str) -> Output {
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{label} failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn retry_busy<T>(mut action: impl FnMut() -> Result<T, StoreError>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match action() {
            Ok(value) => return value,
            Err(StoreError::Busy) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("store operation failed: {error}"),
        }
    }
}

fn seed(fixture: &Fixture, value: &str) -> (MemoryId, Cas) {
    let store = fixture.store();
    let admission = create_user(
        &store,
        MemoryScope::Project,
        "shared_subject",
        value,
        &format!("The shared value is {value}."),
    );
    let id = applied_id(&admission);
    let read = store.read(&id).unwrap();
    (id, read.cas)
}

// ----- helpers ------------------------------------------------------------

#[test]
#[ignore]
fn cross_process_writer_helper() {
    let (Ok(current), Ok(state), Ok(id), Ok(iterations)) = (
        std::env::var("V2_X_CURRENT"),
        std::env::var("V2_X_STATE"),
        std::env::var("V2_X_TARGET"),
        std::env::var("V2_X_ITERATIONS"),
    ) else {
        return;
    };
    let iterations: u32 = iterations.parse().unwrap();
    let id = MemoryId::parse(id).unwrap();
    let store = MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, current, state)).unwrap();
    let caller = Caller::UserCommand {
        invocation: WRITER_CALLER.to_string(),
    };
    for index in 0..iterations {
        let value = format!("v{}", index + 1);
        let read = retry_busy(|| store.read(&id));
        let admission = retry_busy(|| {
            store.update(
                OpId::parse(format!("op-writer-{index}")).unwrap(),
                caller.clone(),
                &id,
                RecordPatch {
                    value: Some(value.clone()),
                    body: Some(format!("The shared value is {value}.")),
                    ..Default::default()
                },
                read.cas.clone(),
            )
        });
        assert!(admission.is_applied(), "{admission:?}");
    }
    let final_read = retry_busy(|| store.read(&id));
    println!("FINAL_REVISION={}", final_read.record.frontmatter.revision);
}

#[test]
#[ignore]
fn cross_process_reader_helper() {
    let (Ok(current), Ok(state), Ok(id), Ok(iterations)) = (
        std::env::var("V2_X_CURRENT"),
        std::env::var("V2_X_STATE"),
        std::env::var("V2_X_TARGET"),
        std::env::var("V2_X_ITERATIONS"),
    ) else {
        return;
    };
    let iterations: u32 = iterations.parse().unwrap();
    let id = MemoryId::parse(id).unwrap();
    let store = MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, current, state)).unwrap();
    let mut previous_revision = 0;
    for _ in 0..iterations {
        let read = retry_busy(|| store.read(&id));
        let revision = read.record.frontmatter.revision;
        assert!(
            revision >= previous_revision,
            "revision regressed from {previous_revision} to {revision}"
        );
        previous_revision = revision;
        let value = read
            .record
            .frontmatter
            .value
            .clone()
            .expect("the seeded record keeps its value");
        assert_eq!(read.record.body, format!("The shared value is {value}."));
        let active = retry_busy(|| store.effective_active());
        assert_eq!(active.len(), 1, "{active:?}");
        assert_eq!(active[0].record.frontmatter.id, id);
        let pending = retry_busy(|| store.list_pending());
        assert!(pending.is_empty(), "{pending:?}");
    }
    println!("READS={iterations}");
}

#[test]
#[ignore]
fn cross_process_stale_cas_helper() {
    let (Ok(current), Ok(state), Ok(id), Ok(revision), Ok(digest)) = (
        std::env::var("V2_X_CURRENT"),
        std::env::var("V2_X_STATE"),
        std::env::var("V2_X_TARGET"),
        std::env::var("V2_X_REVISION"),
        std::env::var("V2_X_DIGEST"),
    ) else {
        return;
    };
    let id = MemoryId::parse(id).unwrap();
    let store = MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, current, state)).unwrap();
    let caller = Caller::UserCommand {
        invocation: STALE_CALLER.to_string(),
    };
    let result = store.update(
        OpId::parse("op-stale-writer").unwrap(),
        caller,
        &id,
        RecordPatch {
            value: Some("stale".into()),
            body: Some("The shared value is stale.".into()),
            ..Default::default()
        },
        Cas {
            revision: revision.parse().unwrap(),
            digest,
        },
    );
    match result {
        Ok(Admission::Conflict { .. }) => println!("STALE=conflict"),
        Err(StoreError::Conflict { .. }) => println!("STALE=conflict"),
        Err(StoreError::ExternalConflict { .. }) => println!("STALE=external_conflict"),
        Ok(admission) => println!("STALE=unexpected:{admission:?}"),
        Err(error) => println!("STALE=unexpected_error:{error}"),
    }
}

// ----- tests --------------------------------------------------------------

#[test]
fn cross_process_readers_never_observe_a_half_committed_record() {
    let fixture = Fixture::new(MemoryScope::Project);
    let (id, _) = seed(&fixture, "v0");
    let iterations = 40u32;
    let target = id.to_string();
    let mut writer = spawn(
        &fixture,
        "cross_process_writer_helper",
        &[
            ("V2_X_TARGET", target.clone()),
            ("V2_X_ITERATIONS", iterations.to_string()),
        ],
    );
    let mut readers = Vec::new();
    for _ in 0..2 {
        readers.push(spawn(
            &fixture,
            "cross_process_reader_helper",
            &[
                ("V2_X_TARGET", target.clone()),
                ("V2_X_ITERATIONS", "120".into()),
            ],
        ));
    }

    // This process is itself a reader for the same scope.
    let store = fixture.store();
    let mut previous_revision = 0;
    let mut reads = 0;
    loop {
        let read = retry_busy(|| store.read(&id));
        let revision = read.record.frontmatter.revision;
        assert!(
            revision >= previous_revision,
            "revision regressed from {previous_revision} to {revision}"
        );
        previous_revision = revision;
        let value = read.record.frontmatter.value.clone().unwrap();
        assert!(value.starts_with('v'), "{value}");
        assert_eq!(read.record.body, format!("The shared value is {value}."));
        assert_eq!(retry_busy(|| store.effective_active()).len(), 1);
        reads += 1;
        if writer.try_wait().unwrap().is_some() {
            break;
        }
    }
    let output = wait_ok(writer, "writer");
    assert!(String::from_utf8_lossy(&output.stdout).contains("FINAL_REVISION="));
    for reader in readers {
        wait_ok(reader, "reader");
    }
    assert!(reads > 0);

    let store = fixture.store();
    let final_read = store.read(&id).unwrap();
    assert_eq!(
        final_read.record.frontmatter.revision,
        u64::from(iterations) + 1,
        "every writer revision must be durable"
    );
    assert_eq!(final_read.record.frontmatter.value.as_deref(), Some("v40"));
    assert_eq!(store.effective_active().unwrap().len(), 1);
}

#[test]
fn a_stale_cas_from_another_process_cannot_overwrite_current_truth() {
    let fixture = Fixture::new(MemoryScope::Project);
    let (id, stale_cas) = seed(&fixture, "v0");
    let store = fixture.store();
    let read = store.read(&id).unwrap();
    let admission = store
        .update(
            OpId::parse("op-current-writer").unwrap(),
            user(),
            &id,
            RecordPatch {
                value: Some("current".into()),
                body: Some("The shared value is current.".into()),
                ..Default::default()
            },
            read.cas,
        )
        .unwrap();
    assert!(admission.is_applied(), "{admission:?}");

    let output = spawn(
        &fixture,
        "cross_process_stale_cas_helper",
        &[
            ("V2_X_TARGET", id.to_string()),
            ("V2_X_REVISION", stale_cas.revision.to_string()),
            ("V2_X_DIGEST", stale_cas.digest.clone()),
        ],
    );
    let output = wait_ok(output, "stale writer");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("STALE=conflict") || stdout.contains("STALE=external_conflict"),
        "stale writer produced: {stdout}"
    );

    let store = fixture.store();
    let current = store.read(&id).unwrap();
    assert_eq!(
        current.record.frontmatter.value.as_deref(),
        Some("current"),
        "the stale writer overwrote a newer value"
    );
}

#[test]
fn an_external_file_edit_is_not_overwritten_by_a_reader_process() {
    let fixture = Fixture::new(MemoryScope::Project);
    let (id, cas) = seed(&fixture, "v0");
    let path = fixture.current_file(&id);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\nAn edit made outside the store.\n");
    std::fs::write(&path, text).unwrap();

    let output = spawn(
        &fixture,
        "cross_process_stale_cas_helper",
        &[
            ("V2_X_TARGET", id.to_string()),
            ("V2_X_REVISION", cas.revision.to_string()),
            ("V2_X_DIGEST", cas.digest.clone()),
        ],
    );
    let output = wait_ok(output, "stale writer");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("STALE=conflict") || stdout.contains("STALE=external_conflict"),
        "stale writer produced: {stdout}"
    );
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after.contains("An edit made outside the store."),
        "the external edit was silently replaced"
    );
}

#[test]
fn a_sigkilled_writer_leaves_a_store_a_new_process_can_recover() {
    let fixture = Fixture::new(MemoryScope::Project);
    let (id, _) = seed(&fixture, "v0");
    let mut writer = spawn(
        &fixture,
        "cross_process_writer_helper",
        &[
            ("V2_X_TARGET", id.to_string()),
            ("V2_X_ITERATIONS", "400".into()),
        ],
    );
    // Let the writer enter its transaction loop, then kill it without a chance
    // to clean up.
    let deadline = Instant::now() + Duration::from_secs(30);
    let store = fixture.store();
    loop {
        let revision = retry_busy(|| store.read(&id)).record.frontmatter.revision;
        if revision > 1 || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    #[cfg(windows)]
    let status = {
        let pid = writer.id().to_string();
        Command::new("taskkill")
            .args(["/F", "/PID", pid.as_str()])
            .status()
            .unwrap()
    };
    #[cfg(not(windows))]
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -9 {}", writer.id()))
        .status()
        .unwrap();
    assert!(status.success());
    let _ = writer.wait();

    let store = fixture.store();
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1, "{active:?}");
    assert_eq!(active[0].record.frontmatter.id, id);
    let read = store.read(&id).unwrap();
    assert!(read.record.frontmatter.revision >= 1);
    let paths = store.paths();
    for entry in std::fs::read_dir(&paths.journal_dir).unwrap() {
        let entry = entry.unwrap();
        assert!(
            !entry.path().is_file(),
            "a journal survived recovery: {:?}",
            entry.path()
        );
    }
    for entry in std::fs::read_dir(&paths.stage_dir).unwrap() {
        let entry = entry.unwrap();
        assert!(
            !entry.path().is_file(),
            "a staged file survived recovery: {:?}",
            entry.path()
        );
    }
}
