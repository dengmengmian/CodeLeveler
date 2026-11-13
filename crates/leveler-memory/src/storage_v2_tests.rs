use std::path::Path;

use tempfile::TempDir;

use crate::storage::MemoryStorage;
use crate::v2::{MAX_MANUAL_OBSERVATION_BYTES, V2StorageError};

fn make_v2_storage(temp: &TempDir) -> MemoryStorage {
    MemoryStorage::for_workspace(
        Path::new("/home/user/project"),
        &temp.path().join("memory-v2"),
    )
}

#[test]
fn v2_scope_paths_live_under_the_configured_root() {
    let temp = TempDir::new().unwrap();
    let storage = make_v2_storage(&temp);

    assert_eq!(
        storage.global_dir(),
        temp.path().join("memory-v2").join("global")
    );
    assert!(
        storage
            .workspace_dir()
            .starts_with(temp.path().join("memory-v2").join("workspaces")),
        "workspace memory must stay under the storage root"
    );
    assert!(
        storage
            .workspace_dir()
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("project-")),
        "workspace memory directory must be slugged from the project name"
    );
}

#[test]
fn remember_notes_are_published_as_distinct_global_observations() {
    let temp = TempDir::new().unwrap();
    let storage = make_v2_storage(&temp);

    storage
        .save_remember_note("Prefer concise explanations")
        .unwrap();
    storage
        .save_remember_note("Deployments\nRequire the staging flag")
        .unwrap();

    let inbox = storage.global_dir().join("observations/_inbox");
    let mut observations: Vec<_> = std::fs::read_dir(&inbox)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("md"))
        .collect();
    observations.sort();
    assert_eq!(observations.len(), 2);
    assert!(
        observations.iter().all(|path| !path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with('.')),
        "published observations must be visible while temporary files stay hidden"
    );
    let contents: Vec<_> = observations
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect();
    assert!(contents.contains(&"## Prefer concise explanations".to_string()));
    assert!(contents.contains(&"## Deployments\n\nRequire the staging flag".to_string()));

    let listed = storage.list_memory_files().unwrap();
    assert!(observations.iter().all(|path| listed.contains(path)));
    storage.ensure_initialized().unwrap();
    let manifest = std::fs::read_to_string(storage.global_memory_file()).unwrap();
    assert!(manifest.contains("Prefer concise explanations"));
    assert!(manifest.contains("Deployments"));
}

#[test]
fn oversized_remember_note_is_rejected_with_typed_error() {
    let temp = TempDir::new().unwrap();
    let storage = make_v2_storage(&temp);
    let oversized = "x".repeat(MAX_MANUAL_OBSERVATION_BYTES + 1);

    let error = storage.save_remember_note(&oversized).unwrap_err();
    assert!(matches!(
        error,
        V2StorageError::ObservationTooLarge {
            actual_bytes,
            limit_bytes: MAX_MANUAL_OBSERVATION_BYTES,
        } if actual_bytes > MAX_MANUAL_OBSERVATION_BYTES
    ));
    assert!(
        !storage.global_dir().join("observations/_inbox").exists(),
        "oversized notes must fail before writing an observation"
    );
}

#[test]
fn v2_listing_enforces_directory_entry_and_file_caps() {
    let temp = TempDir::new().unwrap();
    let storage = make_v2_storage(&temp);
    let topics = storage.global_dir().join("topics");
    std::fs::create_dir_all(&topics).unwrap();
    for index in 0..5 {
        std::fs::write(topics.join(format!("topic-{index}.md")), "# Topic").unwrap();
    }
    let list = |max_entries, max_files| {
        crate::storage_v2::list_memory_files_with_caps(
            storage.global_dir(),
            storage.workspace_dir(),
            max_entries,
            max_files,
        )
    };

    assert_eq!(list(5, 100).unwrap().len(), 5);
    assert_eq!(
        list(4, 100).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData,
        "walking past the entry cap must fail closed"
    );
    assert_eq!(list(100, 3).unwrap().len(), 3);
    assert_eq!(
        list(
            crate::v2::MAX_DIRECTORY_ENTRIES,
            crate::v2::MAX_DISCOVERED_FILES
        )
        .unwrap(),
        storage.list_memory_files().unwrap(),
        "the public listing must use the manifest walker's caps"
    );
}

#[test]
fn v2_listing_omits_hidden_and_tombstoned_observations() {
    let temp = TempDir::new().unwrap();
    let storage = make_v2_storage(&temp);
    storage.ensure_initialized().unwrap();
    crate::V2CaptureStore::open(storage.global_dir(), crate::V2MemoryScope::Global).unwrap();

    let inbox = storage.global_dir().join("observations/_inbox");
    let hidden = inbox.join("hidden.md");
    let tombstoned = inbox.join("tombstoned.md");
    let visible = inbox.join("visible.md");
    for path in [&hidden, &tombstoned, &visible] {
        std::fs::write(path, "# Observation").unwrap();
    }
    let state_path = storage.global_dir().join("memory_state.sqlite");
    let connection = crate::sqlite_journal::JournalMode::for_db_path(&state_path)
        .open(&state_path)
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_hidden_observations(relative_path)
             VALUES ('observations/_inbox/hidden.md')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO memory_v2_tombstones(
                tombstone_id, target_kind, relative_path, provenance_hash, created_at, reason
             ) VALUES ('forget-test', 'observation', 'observations/_inbox/tombstoned.md',
                       ?1, 1, 'privacy')",
            [blake3::hash(b"# Observation").to_hex().to_string()],
        )
        .unwrap();
    drop(connection);

    let listed = storage.list_memory_files().unwrap();
    assert!(listed.contains(&visible));
    assert!(
        !listed.contains(&hidden),
        "hidden observation was browsable"
    );
    assert!(
        !listed.contains(&tombstoned),
        "tombstoned observation was browsable"
    );
}

#[cfg(unix)]
#[test]
fn v2_listing_rejects_symlinked_source_directories() {
    use std::os::unix::fs::symlink;

    for relative in ["topics", "observations/_inbox"] {
        let temp = TempDir::new().unwrap();
        let storage = make_v2_storage(&temp);
        storage.ensure_initialized().unwrap();

        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("escaped.md"), "# Outside").unwrap();
        let directory = storage.workspace_dir().join(relative);
        std::fs::remove_dir(&directory).unwrap();
        symlink(&outside, &directory).unwrap();

        assert_eq!(
            storage.list_memory_files().unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied,
            "{relative} symlink must be rejected"
        );
    }
}

#[cfg(unix)]
#[test]
fn v2_listing_rejects_symlinked_markdown_files() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let storage = make_v2_storage(&temp);
    storage.ensure_initialized().unwrap();

    let outside = temp.path().join("outside.md");
    std::fs::write(&outside, "# Outside").unwrap();
    symlink(&outside, storage.workspace_dir().join("topics/escaped.md")).unwrap();

    assert_eq!(
        storage.list_memory_files().unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[cfg(unix)]
#[test]
fn v2_listing_rejects_symlinked_workspace_root() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let storage = make_v2_storage(&temp);
    storage.ensure_initialized().unwrap();

    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.md"), "# Outside").unwrap();
    std::fs::remove_dir_all(storage.workspace_dir()).unwrap();
    symlink(&outside, storage.workspace_dir()).unwrap();

    assert_eq!(
        storage.list_memory_files().unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[test]
fn listing_follows_manifest_order_within_each_scope() {
    let temp = TempDir::new().unwrap();
    let storage = make_v2_storage(&temp);
    storage.ensure_initialized().unwrap();
    let scope = storage.workspace_dir().to_path_buf();
    for slug in ["alpha", "beta", "gamma"] {
        std::fs::write(
            scope.join(format!("topics/{slug}.md")),
            format!("# {slug}\n\nBody."),
        )
        .unwrap();
    }
    let older = scope.join("observations/_inbox/older.md");
    let newer = scope.join("observations/_inbox/newer.md");
    std::fs::write(&older, "# Older").unwrap();
    std::fs::write(&newer, "# Newer").unwrap();
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&older)
        .unwrap()
        .set_modified(hour_ago)
        .unwrap();
    let relative = |files: &[std::path::PathBuf]| -> Vec<String> {
        files
            .iter()
            .filter_map(|path| path.strip_prefix(&scope).ok())
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect()
    };

    // Default index: the manifest, alphabetical topics, then inbox notes newest first.
    let listed = storage.list_memory_files().unwrap();
    assert_eq!(
        relative(&listed),
        [
            "MEMORY.md",
            "topics/alpha.md",
            "topics/beta.md",
            "topics/gamma.md",
            "observations/_inbox/newer.md",
            "observations/_inbox/older.md",
        ]
    );
}
