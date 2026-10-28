//! Current root and private state root on different filesystems.
//!
//! The commit protocol writes every staged file beside its final path, so no
//! rename crosses a device boundary. These tests prove that claim by placing
//! the two roots on different mounted filesystems and walking the lifecycle.
//!
//! Only macOS can mount a volume unprivileged here, so the whole test crate is
//! compiled away elsewhere: cross-device memory is NOT VERIFIED on other hosts.

#![cfg(target_os = "macos")]

mod memory_v2_common;

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use leveler_memory::v2::{
    Cas, ForgetOutcome, MarkdownStore, MemoryId, MemoryScope, RecordPatch, ScopeRoots, StoreError,
};
use memory_v2_common::*;

#[cfg(target_os = "macos")]
use memory_v2_common::MountedVolume;

fn device(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().dev()
}

fn create(store: &MarkdownStore, index: u32) -> (MemoryId, Cas) {
    let admission = create_user(
        store,
        MemoryScope::Project,
        &format!("cross_fs_subject_{index}"),
        &format!("value-{index}"),
        &format!("The value is value-{index}."),
    );
    let id = applied_id(&admission);
    let read = store.read(&id).unwrap();
    (id, read.cas)
}

fn exercise(store: &MarkdownStore) {
    // Create.
    let (id, first) = create(store, 0);
    assert_eq!(store.read(&id).unwrap().record.frontmatter.revision, 1);

    // Update in place, which keeps history on the other device.
    let second = {
        let admission = store
            .update(
                op(),
                user(),
                &id,
                RecordPatch {
                    value: Some("value-updated".into()),
                    body: Some("The value is value-updated.".into()),
                    ..Default::default()
                },
                first,
            )
            .unwrap();
        assert!(admission.is_applied(), "{admission:?}");
        store.read(&id).unwrap().cas
    };
    assert_eq!(store.read(&id).unwrap().record.frontmatter.revision, 2);

    // Supersede by a second record, then forget the updated one.
    let (other, _other_cas) = create(store, 1);
    let admission = store
        .supersede(
            op(),
            user(),
            new_record_for(&fact_intent(
                MemoryScope::Project,
                Some("cross_fs_subject_1"),
                "value-replaced",
                "The value is value-replaced.",
            )),
            std::slice::from_ref(&other),
        )
        .unwrap();
    assert!(admission.is_applied(), "{admission:?}");
    assert!(!store.paths().current_file(&other).exists());

    let outcome = store.forget(op(), user(), &id, second).unwrap();
    assert!(
        matches!(outcome, ForgetOutcome::Forgotten(_)),
        "{outcome:?}"
    );
    assert!(matches!(store.read(&id), Err(StoreError::Forgotten(_))));
}

#[cfg(target_os = "macos")]
#[test]
fn current_on_a_different_device_from_state_supports_the_lifecycle() {
    let Some(volume) = MountedVolume::new("current") else {
        eprintln!("hdiutil could not mount a volume; cross-device current NOT_VERIFIED");
        return;
    };
    let state_dir = tempfile::tempdir().unwrap();
    let current = volume.path().join("memory");
    let state = state_dir.path().join("state");
    let store =
        MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, &current, &state)).unwrap();
    assert_ne!(
        device(&store.paths().current_root),
        device(&store.paths().state_root),
        "the test premise requires two devices"
    );
    exercise(&store);
    drop(store);

    let store =
        MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, &current, &state)).unwrap();
    assert_eq!(store.effective_active().unwrap().len(), 1);
    let report = store.recover().unwrap();
    assert!(!report.is_blocked(), "{report:?}");
}

#[cfg(target_os = "macos")]
#[test]
fn state_on_a_different_device_from_current_supports_the_lifecycle() {
    let Some(volume) = MountedVolume::new("state") else {
        eprintln!("hdiutil could not mount a volume; cross-device state NOT_VERIFIED");
        return;
    };
    let current_dir = tempfile::tempdir().unwrap();
    let current = current_dir.path().join("memory");
    let state = volume.path().join("state");
    let store =
        MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, &current, &state)).unwrap();
    assert_ne!(
        device(&store.paths().current_root),
        device(&store.paths().state_root),
        "the test premise requires two devices"
    );
    exercise(&store);
    drop(store);

    let store =
        MarkdownStore::open(ScopeRoots::new(MemoryScope::Project, &current, &state)).unwrap();
    let active = store.effective_active().unwrap();
    assert_eq!(active.len(), 1);
    assert!(store.read(&active[0].record.frontmatter.id).is_ok());
}
