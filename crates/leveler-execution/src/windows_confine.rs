//! Windows write confinement: the launcher and the write-root leases.
//!
//! CodeLeveler's filesystem model restricts writes and leaves reads alone
//! (see [`crate::WriteScope`]). On Windows the primitive that says exactly
//! that is Mandatory Integrity Control: the child runs at Low integrity, which
//! denies write-up and never denies read-up, and each authorized write root is
//! labelled Low for the life of the command so the child can write there.
//!
//! What MIC gives is narrower than a write allowlist, and the difference is
//! worth stating: a Low child cannot write an object labelled Medium or above,
//! which is every ordinary file on the host. It says nothing about objects that
//! already carry a Low label for reasons of their own. "Authorized roots are
//! writable, ordinary files are not" is the guarantee; "nowhere else" is not.
//!
//! Three pieces live here:
//! - [`launcher_path`] finds `leveler-confine.exe`, the argv wrapper that
//!   lowers the token — the Windows counterpart of `sandbox-exec` / `bwrap`.
//! - [`lease_write_roots`] labels the write roots and puts their labels back
//!   when the command ends.
//! - [`recover_stale_write_roots`] puts them back after a run that never got
//!   to. A lowered label outlives the process that lowered it, so the record
//!   that describes it has to be durable and has to be acted on without
//!   waiting for the same repository to be opened again.
//!
//! The module compiles on every host so its bookkeeping is unit-tested
//! everywhere; the Win32 calls behind it are Windows-only.

use std::collections::HashMap;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use leveler_win_confine::IntegrityLabel;

/// The name of the launcher binary, next to the running executable.
const LAUNCHER_NAME: &str = "leveler-confine.exe";

/// Override for hosts that install the launcher somewhere else.
const LAUNCHER_ENV: &str = "LEVELER_CONFINE_BIN";

/// Absolute path to `leveler-confine.exe`, or `None` when this installation
/// does not carry one.
///
/// Resolution, in order: `LEVELER_CONFINE_BIN`, the directory of the running
/// executable (an installed `leveler.exe` ships the launcher beside it), then
/// its parent (a `cargo test` binary lives in `target/<profile>/deps`, one
/// level below where Cargo puts workspace binaries).
pub fn launcher_path() -> Option<&'static Path> {
    static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();
    RESOLVED.get_or_init(resolve_launcher).as_deref()
}

fn resolve_launcher() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os(LAUNCHER_ENV) {
        let configured = PathBuf::from(configured);
        return configured.is_file().then_some(configured);
    }
    let executable = std::env::current_exe().ok()?;
    let mut directory = executable.parent();
    // The executable's own directory, then one level up. Two candidates is the
    // whole search: further up is someone else's `target/`, not ours.
    for _ in 0..2 {
        let candidate = directory?.join(LAUNCHER_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        directory = directory?.parent();
    }
    None
}

/// A held claim on one write root: how many commands are writing under it, and
/// the label to put back when the last one finishes.
#[derive(Debug, Clone)]
struct Held {
    count: usize,
    previous: IntegrityLabel,
}

fn held() -> &'static Mutex<HashMap<String, Held>> {
    static HELD: OnceLock<Mutex<HashMap<String, Held>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(HashMap::new()))
}

fn root_key(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .to_ascii_lowercase()
}

/// One root this lease lowered, and the record that says how to put it back.
/// The record path is kept rather than recomputed: it is derived from the
/// canonical path, and a root deleted mid-command no longer canonicalizes to
/// the same thing.
#[derive(Debug)]
struct LeasedRoot {
    root: PathBuf,
    record: PathBuf,
}

/// Labels released when the command that claimed them ends. Held by the
/// spawned [`crate::command::ManagedProcess`], so a background task keeps its
/// write roots writable for exactly as long as it runs.
#[derive(Debug)]
pub struct WriteRootLease {
    roots: Vec<LeasedRoot>,
}

impl Drop for WriteRootLease {
    fn drop(&mut self) {
        for leased in std::mem::take(&mut self.roots) {
            if let Err(error) = release_root(&leased.root, &leased.record) {
                tracing::warn!(
                    %error,
                    root = %leased.root.display(),
                    "failed to restore a write root's integrity label"
                );
            }
        }
    }
}

/// Label every root in `roots` Low so a Low-integrity child may write there.
/// Fails closed: if any root cannot be labelled, the ones already taken are
/// released and the command does not run.
///
/// Only the user's own roots are restored afterwards. CodeLeveler's private
/// scratch and per-workspace tool cache exist to be written by confined
/// commands and nothing else, so they keep their label — relabelling a Cargo
/// registry twice per command would cost far more than it protects.
pub fn lease_write_roots(
    environment: &leveler_core::EnvSnapshot,
    roots: &[PathBuf],
) -> io::Result<WriteRootLease> {
    let records = records_dir(environment);
    std::fs::create_dir_all(&records)?;
    // Nothing in this process holds a lease yet the first time through, so
    // recovery never races an active one. A failure here fails the command
    // rather than running it over label state we could not put straight.
    ensure_recovered(&records)?;

    let home = leveler_core::LevelerHome::resolve(environment);
    let mut lease = WriteRootLease { roots: Vec::new() };
    for root in roots {
        // A root CodeLeveler owns is never restored, so it needs no record —
        // and a record for it would tell recovery to undo a label that is
        // supposed to stay.
        let restorable = !is_leveler_owned(root, home.root());
        let record = acquire_root(root, &records, restorable)?;
        if let Some(record) = record {
            lease.roots.push(LeasedRoot {
                root: root.clone(),
                record,
            });
        }
    }
    Ok(lease)
}

/// Whether `root` is one of CodeLeveler's own directories rather than the
/// user's. Compared component-wise on the normalized keys, so a symlinked or
/// differently-cased home matches and `<home>-old` does not.
fn is_leveler_owned(root: &Path, home: &Path) -> bool {
    Path::new(&root_key(root)).starts_with(Path::new(&root_key(home)))
}

/// Where a root's pre-label state is parked while it is labelled. Not in the
/// workspace: a residue file the agent can edit is not a record of anything,
/// and a repo is not our scratch space.
fn records_dir(environment: &leveler_core::EnvSnapshot) -> PathBuf {
    leveler_core::LevelerHome::resolve(environment)
        .run_dir()
        .join("windows-write-roots")
}

fn record_path(records: &Path, root: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(root_key(root).as_bytes());
    let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    records.join(format!("{name}.label"))
}

/// Claim `root`. Returns the record that describes how to put its label back,
/// or `None` when this root is not one we ever restore.
///
/// The record is durable **before** the label moves. The other order would
/// leave a lowered root with nothing on disk saying what it used to be, which
/// is precisely the state no later run could repair.
fn acquire_root(root: &Path, records: &Path, restorable: bool) -> io::Result<Option<PathBuf>> {
    let key = root_key(root);
    let record = restorable.then(|| record_path(records, root));
    let mut map = held().lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(entry) = map.get_mut(&key) {
        entry.count += 1;
        return Ok(record);
    }
    let previous = leveler_win_confine::integrity_label(root)?;
    if let Some(record) = record.as_deref() {
        write_record(record, &Record::new(root, previous.clone()))?;
    }
    leveler_win_confine::apply_low_integrity_label(root)?;
    map.insert(key, Held { count: 1, previous });
    Ok(record)
}

fn release_root(root: &Path, record: &Path) -> io::Result<()> {
    let key = root_key(root);
    let previous = {
        let mut map = held().lock().unwrap_or_else(|poison| poison.into_inner());
        let Some(entry) = map.get_mut(&key) else {
            return Ok(());
        };
        entry.count -= 1;
        if entry.count > 0 {
            return Ok(());
        }
        map.remove(&key).map(|entry| entry.previous)
    };
    let Some(previous) = previous else {
        return Ok(());
    };
    let restored = leveler_win_confine::restore_integrity_label(root, &previous);
    // Only drop the record once the label is actually back, so a failure here
    // still leaves the next run enough to recover from.
    if restored.is_ok() {
        let _ = std::fs::remove_file(record);
    }
    restored
}

/// One thing recovery has to do to converge a leftover record.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recovery {
    /// The root is still there: put its label back, then drop the record.
    Restore { record: PathBuf, entry: Record },
    /// The root is gone. There is no label left to restore and no object to
    /// restore it onto, so the record has outlived what it described.
    Forget { record: PathBuf },
}

/// Whether `text` is a record in the shape that came before [`Record`]: a bare
/// label and nothing else. Deliberately exact, so a corrupt record cannot pass
/// for one and be thrown away.
fn is_record_without_a_root(text: &str) -> bool {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let Some(only) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false;
    }
    let mut parts = only.split_whitespace();
    match parts.next() {
        Some("inherited") => parts.next().is_none(),
        Some("explicit") => {
            let sid = parts.next().is_some_and(|sid| sid.starts_with("S-"));
            let flags = parts.next().is_some_and(|part| part.parse::<u8>().is_ok());
            let mask = parts.next().is_some_and(|part| part.parse::<u32>().is_ok());
            sid && flags && mask && parts.next().is_none()
        }
        _ => false,
    }
}

/// Recovery stops rather than inventing a label for a record it cannot read:
/// the file says a root was lowered, and guessing what it used to be would
/// write an integrity level onto a user's directory that nobody ever chose.
fn unreadable_records(paths: &[PathBuf]) -> io::Error {
    let names: Vec<String> = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "cannot read {} Windows write-root recovery record(s), so the integrity \
             label they describe cannot be put back and confined execution is refused. \
             Inspect and remove them once the roots they name are known to be correct: {}",
            names.len(),
            names.join(", ")
        ),
    )
}

/// Decide what every leftover record needs, without touching a label.
///
/// Pure enough to test on any host: the Win32 calls are in
/// [`recover_stale_write_roots`], which applies what this returns.
fn plan_recovery(records: &Path) -> io::Result<Vec<Recovery>> {
    let entries = match std::fs::read_dir(records) {
        Ok(entries) => entries,
        // Nothing has ever been lowered on this host.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut plan = Vec::new();
    let mut unreadable = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "label")
        {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // A record we cannot even read is as opaque as one we cannot parse.
            Err(_) => {
                unreadable.push(path);
                continue;
            }
        };
        match Record::decode(&text) {
            Some(entry) => {
                let gone = matches!(entry.root.try_exists(), Ok(false));
                plan.push(if gone {
                    Recovery::Forget { record: path }
                } else {
                    Recovery::Restore {
                        record: path,
                        entry,
                    }
                });
            }
            // A record from the build that wrote no root path. It cannot say
            // which directory it was about, so it is dropped rather than left
            // to refuse every confined command forever. The build that wrote
            // it shipped no Windows binary, so the window it covers is one
            // developer machine deep.
            None if is_record_without_a_root(&text) => plan.push(Recovery::Forget { record: path }),
            None => unreadable.push(path),
        }
    }
    if !unreadable.is_empty() {
        return Err(unreadable_records(&unreadable));
    }
    plan.sort_by(|left, right| record_of(left).cmp(record_of(right)));
    Ok(plan)
}

fn record_of(recovery: &Recovery) -> &Path {
    match recovery {
        Recovery::Restore { record, .. } | Recovery::Forget { record } => record,
    }
}

/// Put back every write root a previous run lowered and never restored.
///
/// A run killed between lowering a root and restoring it leaves the root Low
/// and its record on disk. Both outlive the process, so this reads the records
/// rather than waiting for the same root to be leased again — a repository the
/// user never opens in CodeLeveler again would otherwise stay Low forever.
///
/// Idempotent by construction. Restoring a label that is already the recorded
/// one writes the same label; the record is removed only once the restore has
/// actually succeeded, so a run interrupted between the two simply repeats the
/// restore next time.
///
/// Off Windows there are no integrity labels to put back and this does
/// nothing.
pub fn recover_stale_write_roots(environment: &leveler_core::EnvSnapshot) -> io::Result<()> {
    recover_records(&records_dir(environment))
}

fn recover_records(records: &Path) -> io::Result<()> {
    if !cfg!(windows) {
        return Ok(());
    }
    let plan = plan_recovery(records)?;
    let mut failures: Vec<String> = Vec::new();
    for step in plan {
        match step {
            Recovery::Forget { record } => {
                let _ = std::fs::remove_file(record);
            }
            Recovery::Restore { record, entry } => {
                match leveler_win_confine::restore_integrity_label(&entry.root, &entry.previous) {
                    // The record goes only after the label is back, so an
                    // interrupted recovery repeats rather than forgets.
                    Ok(()) => {
                        let _ = std::fs::remove_file(&record);
                    }
                    // The root disappeared between planning and restoring:
                    // there is nothing left to put a label on.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        let _ = std::fs::remove_file(&record);
                    }
                    Err(error) => {
                        failures.push(format!("{}: {error}", entry.root.display()));
                    }
                }
            }
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "could not restore the integrity label of {} Windows write root(s) a previous \
         run left lowered; their records are kept for the next attempt: {}",
        failures.len(),
        failures.join(", ")
    )))
}

/// Run recovery once per process, before the first write root is leased.
///
/// A failure is not cached: the next confined command tries again, and until
/// one succeeds every confined command is refused. Windows confined execution
/// is what becomes unavailable, not the whole product — but it never proceeds
/// while stale label state is still out there unaccounted for.
fn ensure_recovered(records: &Path) -> io::Result<()> {
    static DONE: OnceLock<Mutex<bool>> = OnceLock::new();
    let gate = DONE.get_or_init(|| Mutex::new(false));
    let mut done = gate.lock().unwrap_or_else(|poison| poison.into_inner());
    if *done {
        return Ok(());
    }
    recover_records(records)?;
    *done = true;
    Ok(())
}

/// What one lowered write root needs for someone else to put it back.
///
/// The root's own path is in the file rather than only in its name: recovery
/// has to work from the records alone, without being told which repository to
/// look at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    root: PathBuf,
    previous: IntegrityLabel,
}

/// Bumped only if the fields change meaning. A record this run cannot read is
/// never guessed at — see [`read_record`].
const RECORD_VERSION: &str = "leveler-write-root 1";

impl Record {
    fn new(root: &Path, previous: IntegrityLabel) -> Self {
        Self {
            // The canonical spelling, so recovery restores the same object
            // whatever spelling the caller used. Not lowercased: NTFS keeps
            // case, and a record is evidence, not a lookup key.
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            previous,
        }
    }

    fn encode(&self) -> String {
        let label = match &self.previous {
            IntegrityLabel::Inherited => "inherited".to_string(),
            IntegrityLabel::Explicit {
                sid,
                ace_flags,
                mask,
            } => format!("explicit {sid} {ace_flags} {mask}"),
        };
        // `root` last and to end-of-line: a Windows path may hold spaces, and
        // it may not hold a newline.
        format!(
            "{RECORD_VERSION}\nlabel {label}\nroot {}\n",
            self.root.display()
        )
    }

    fn decode(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()?.trim_end() != RECORD_VERSION {
            return None;
        }
        let previous = match lines.next()?.trim_end().strip_prefix("label ")? {
            "inherited" => IntegrityLabel::Inherited,
            explicit => {
                let mut parts = explicit.strip_prefix("explicit ")?.split_whitespace();
                IntegrityLabel::Explicit {
                    sid: parts.next()?.to_string(),
                    ace_flags: parts.next()?.parse().ok()?,
                    mask: parts.next()?.parse().ok()?,
                }
            }
        };
        let root = lines.next()?.trim_end().strip_prefix("root ")?;
        if root.is_empty() {
            return None;
        }
        Some(Self {
            root: PathBuf::from(root),
            previous,
        })
    }
}

/// Write a record and get it onto the disk before returning. Without the sync
/// the label can be lowered while the record that undoes it is still only in a
/// cache, which is the one ordering this whole mechanism exists to prevent.
fn write_record(path: &Path, record: &Record) -> io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(record.encode().as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(root: &str, previous: IntegrityLabel) -> Record {
        Record {
            root: PathBuf::from(root),
            previous,
        }
    }

    #[test]
    fn a_record_survives_its_round_trip_with_the_root_it_describes() {
        for entry in [
            record("/repo", IntegrityLabel::Inherited),
            record(
                r"C:\Users\me\a project",
                IntegrityLabel::Explicit {
                    sid: "S-1-16-4096".to_string(),
                    ace_flags: 3,
                    mask: 1,
                },
            ),
        ] {
            let encoded = entry.encode();
            assert_eq!(Record::decode(&encoded), Some(entry), "{encoded}");
        }
    }

    /// The root has to come out of the record itself. Recovery runs from the
    /// records alone — nobody tells it which repository to look at.
    #[test]
    fn a_record_carries_the_root_it_describes() {
        let entry = record(r"C:\Users\me\proj", IntegrityLabel::Inherited);
        assert!(entry.encode().contains(r"C:\Users\me\proj"));
        assert_eq!(Record::decode(&entry.encode()).unwrap().root, entry.root);
    }

    #[test]
    fn a_record_this_run_cannot_read_decodes_to_nothing_rather_than_a_guess() {
        for text in [
            "",
            "something-else",
            // A previous format, with no version line.
            "explicit S-1-16-4096 3 1",
            // A version this build does not know.
            "leveler-write-root 99\nlabel inherited\nroot /repo\n",
            // Truncated mid-write.
            "leveler-write-root 1\nlabel inheri",
            // Label present, root missing.
            "leveler-write-root 1\nlabel inherited\n",
            // Root line present but empty.
            "leveler-write-root 1\nlabel inherited\nroot \n",
            // An explicit label missing its mask.
            "leveler-write-root 1\nlabel explicit S-1-16-4096 3\nroot /repo\n",
        ] {
            assert_eq!(Record::decode(text), None, "{text:?}");
        }
    }

    #[test]
    fn only_the_users_own_roots_are_restored() {
        let home = Path::new("/leveler-home");
        assert!(is_leveler_owned(
            Path::new("/leveler-home/cache/tools/abc"),
            home
        ));
        assert!(is_leveler_owned(Path::new("/leveler-home"), home));
        assert!(!is_leveler_owned(Path::new("/Users/me/project"), home));
        // A sibling that merely starts with the same characters is not inside.
        assert!(!is_leveler_owned(Path::new("/leveler-home-old"), home));
    }

    #[test]
    fn each_root_gets_its_own_record_file() {
        let records = Path::new("/records");
        let one = record_path(records, Path::new("/a"));
        let two = record_path(records, Path::new("/b"));
        assert_ne!(one, two);
        assert_eq!(one.parent(), Some(records));
        assert!(
            one.extension()
                .is_some_and(|extension| extension == "label"),
            "{one:?}"
        );
    }

    fn records_dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("records dir");
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).expect("write record");
        }
        dir
    }

    /// C3, the case this whole mechanism exists for: a run died with the root
    /// still lowered, and the record is all that is left of what it was.
    #[test]
    fn a_leftover_record_for_a_live_root_plans_a_restore() {
        let root = tempfile::tempdir().expect("root");
        let entry = Record {
            root: root.path().to_path_buf(),
            previous: IntegrityLabel::Inherited,
        };
        let records = records_dir_with(&[("a.label", &entry.encode())]);

        let plan = plan_recovery(records.path()).expect("scan");
        assert_eq!(plan.len(), 1);
        match &plan[0] {
            Recovery::Restore {
                entry: planned,
                record,
            } => {
                assert_eq!(planned, &entry);
                assert_eq!(record, &records.path().join("a.label"));
            }
            other => panic!("expected a restore, got {other:?}"),
        }
    }

    /// C4 and the repeat case: once the root is gone there is no label left to
    /// put back, and the record has outlived what it described.
    #[test]
    fn a_record_for_a_root_that_no_longer_exists_is_forgotten() {
        let missing = tempfile::tempdir().expect("root");
        let path = missing.path().to_path_buf();
        drop(missing);
        let entry = Record {
            root: path,
            previous: IntegrityLabel::Inherited,
        };
        let records = records_dir_with(&[("a.label", &entry.encode())]);

        let plan = plan_recovery(records.path()).expect("scan");
        assert!(
            matches!(plan.as_slice(), [Recovery::Forget { .. }]),
            "{plan:?}"
        );
    }

    /// R5. Recovery refuses rather than writing an integrity level onto a
    /// user's directory that nobody chose, and it names the file to look at.
    #[test]
    fn an_unreadable_record_refuses_instead_of_guessing_a_label() {
        let records = records_dir_with(&[("broken.label", "not a record at all")]);
        let message = plan_recovery(records.path())
            .expect_err("must not plan anything")
            .to_string();
        assert!(message.contains("broken.label"), "{message}");
        assert!(
            message.to_lowercase().contains("refused"),
            "the refusal must say confined execution is refused: {message}"
        );
        // And the record stays, so a human can still see what was lowered.
        assert!(records.path().join("broken.label").is_file());
    }

    /// One bad record does not get lost behind a good one, whichever order the
    /// directory hands them back in.
    #[test]
    fn one_unreadable_record_refuses_the_whole_pass() {
        let root = tempfile::tempdir().expect("root");
        let good = Record {
            root: root.path().to_path_buf(),
            previous: IntegrityLabel::Inherited,
        };
        let records = records_dir_with(&[("a.label", &good.encode()), ("b.label", "garbage")]);
        assert!(
            plan_recovery(records.path()).is_err(),
            "a readable sibling must not excuse an unreadable record"
        );
    }

    /// The build before this one wrote a label with no root in it. Recovery
    /// cannot act on that, and refusing over it would lock confined execution
    /// out on every machine that ran the previous build.
    #[test]
    fn a_record_from_the_previous_format_is_dropped_rather_than_refused() {
        for legacy in ["inherited\n", "explicit S-1-16-4096 3 1\n"] {
            let records = records_dir_with(&[("old.label", legacy)]);
            let plan = plan_recovery(records.path())
                .unwrap_or_else(|error| panic!("{legacy:?} must not refuse: {error}"));
            assert!(
                matches!(plan.as_slice(), [Recovery::Forget { .. }]),
                "{legacy:?} -> {plan:?}"
            );
        }
    }

    /// And the allowance is exact: garbage must not pass for the old format
    /// and get thrown away in silence.
    #[test]
    fn only_the_real_previous_format_is_dropped() {
        for not_legacy in [
            "inherited extra",
            "explicit S-1-16-4096 3",
            "explicit S-1-16-4096 3 1 4",
            "explicit notasid 3 1",
            "inherited\ninherited",
            "garbage",
            "",
        ] {
            assert!(
                !is_record_without_a_root(not_legacy),
                "{not_legacy:?} is not the previous format"
            );
        }
        assert!(is_record_without_a_root("inherited"));
        assert!(is_record_without_a_root("explicit S-1-16-4096 3 1\n"));
    }

    #[test]
    fn files_that_are_not_records_are_left_alone() {
        let records = records_dir_with(&[("notes.txt", "garbage"), ("README", "garbage")]);
        let plan = plan_recovery(records.path()).expect("scan");
        assert!(plan.is_empty(), "{plan:?}");
    }

    /// C1: a host that never lowered anything has nothing to recover, and the
    /// absence of the directory is not a failure.
    #[test]
    fn a_host_with_no_records_recovers_nothing() {
        let empty = tempfile::tempdir().expect("dir");
        let never_used = empty.path().join("windows-write-roots");
        assert!(plan_recovery(&never_used).expect("scan").is_empty());
        recover_records(&never_used).expect("a host with no records must not fail");
    }

    /// C2: the record is durable before the label moves, so recovery can see a
    /// record for a root that was never actually lowered. Restoring the label
    /// it already has is the same work as restoring one that moved.
    #[test]
    fn a_record_written_before_the_label_moved_is_still_safe_to_act_on() {
        let root = tempfile::tempdir().expect("root");
        let entry = Record {
            root: root.path().to_path_buf(),
            previous: IntegrityLabel::Inherited,
        };
        let records = records_dir_with(&[("a.label", &entry.encode())]);
        let first = plan_recovery(records.path()).expect("scan");
        let second = plan_recovery(records.path()).expect("scan");
        assert_eq!(first, second, "planning twice must plan the same thing");
    }

    #[test]
    fn a_written_record_is_readable_again() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("one.label");
        let entry = Record {
            root: PathBuf::from(r"C:\Users\me\proj"),
            previous: IntegrityLabel::Explicit {
                sid: "S-1-16-8192".to_string(),
                ace_flags: 3,
                mask: 1,
            },
        };
        write_record(&path, &entry).expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(Record::decode(&text), Some(entry));
    }

    #[test]
    fn the_launcher_is_only_reported_when_it_is_really_there() {
        // The env override must name a real file; a missing one is not a
        // launcher, and reporting it would turn fail-closed into a spawn error
        // much later, with a worse message.
        let resolved = launcher_path();
        if let Some(path) = resolved {
            assert!(path.is_file(), "{path:?}");
        }
    }
}

/// Real Windows confinement canaries. These do not assert on a capability
/// probe: they run a process and look at what it could actually read, write
/// and not write. CI runs them as their own required step.
#[cfg(windows)]
#[cfg(test)]
mod windows_canaries {
    use std::path::PathBuf;

    use tokio_util::sync::CancellationToken;

    use crate::command::{CommandRunner, ProcessRequest};
    use crate::risk::WriteScope;

    fn confined(script: &str, workspace: &std::path::Path) -> ProcessRequest {
        let mut request = ProcessRequest::new(
            "cmd",
            vec!["/C".into(), script.into()],
            workspace.to_path_buf(),
        );
        request.write_scope = WriteScope::Workspace {
            root: workspace.to_path_buf(),
        };
        request
    }

    /// A runner that sees the real host environment. `CommandRunner::new()`
    /// reads the installed process snapshot, which a unit test never installs —
    /// it would resolve no home, no PATH and no temp directory.
    fn host_runner() -> CommandRunner {
        CommandRunner::with_environment(std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        )))
    }

    fn host_registry() -> crate::background::BackgroundTaskRegistry {
        crate::background::BackgroundTaskRegistry::with_environment(std::sync::Arc::new(
            leveler_core::EnvSnapshot::new(
                std::env::vars_os(),
                std::env::current_dir().unwrap_or_default(),
                std::env::temp_dir(),
            ),
        ))
    }

    fn launcher_or_skip() -> bool {
        if super::launcher_path().is_none() {
            eprintln!(
                "skipping: leveler-confine.exe is not built \
                 (cargo build -p leveler-win-confine)"
            );
            return false;
        }
        true
    }

    /// The whole point of the backend: a coding agent on Windows must still be
    /// able to run the toolchain that lives outside its workspace.
    #[tokio::test]
    async fn a_confined_command_still_reads_and_runs_the_host_toolchain() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let runner = host_runner();
        let output = runner
            .run(
                confined("cargo --version", workspace.path()),
                CancellationToken::new(),
            )
            .await
            .expect("confined cargo must run");
        assert_eq!(
            output.exit_code,
            Some(0),
            "cargo --version under confinement: {output:?}"
        );
        assert!(
            output.stdout.contains("cargo"),
            "confined command must see the real cargo: {output:?}"
        );
    }

    #[tokio::test]
    async fn a_confined_command_writes_inside_its_workspace() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let runner = host_runner();
        let output = runner
            .run(
                confined("echo inside> inside.txt", workspace.path()),
                CancellationToken::new(),
            )
            .await
            .expect("confined write must run");
        assert_eq!(output.exit_code, Some(0), "{output:?}");
        assert!(workspace.path().join("inside.txt").is_file());
    }

    /// A real repository is not an empty directory. The label has to reach the
    /// files and subdirectories that were already there, or the agent can
    /// create new files and edit nothing it was asked to edit.
    #[tokio::test]
    async fn a_confined_command_edits_files_that_were_already_in_the_workspace() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let nested = workspace.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).expect("nested");
        let existing = nested.join("existing.txt");
        std::fs::write(&existing, "before\n").expect("seed");

        let runner = host_runner();
        let output = runner
            .run(
                confined(
                    "echo after>> src\\deep\\existing.txt && echo new> src\\deep\\new.txt",
                    workspace.path(),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("confined edit must run");
        assert_eq!(output.exit_code, Some(0), "{output:?}");
        let body = std::fs::read_to_string(&existing).expect("read back");
        assert!(
            body.contains("before") && body.contains("after"),
            "a pre-existing file must stay editable under confinement: {body:?}"
        );
        assert!(nested.join("new.txt").is_file());
    }

    #[tokio::test]
    async fn a_confined_command_cannot_write_outside_its_workspace() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("escape.txt");
        let runner = host_runner();
        let output = runner
            .run(
                confined(
                    &format!("echo escaped> \"{}\"", target.display()),
                    workspace.path(),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("the command itself must run");
        assert_ne!(output.exit_code, Some(0), "the write must fail: {output:?}");
        assert!(
            !target.exists(),
            "a confined command wrote outside its workspace: {}",
            target.display()
        );
    }

    /// The label is a change to the user's repository, so it has to come back.
    #[tokio::test]
    async fn a_write_root_gets_its_label_back_when_the_command_ends() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let before = leveler_win_confine::integrity_label(workspace.path()).expect("label before");
        let runner = host_runner();
        runner
            .run(
                confined("echo done> done.txt", workspace.path()),
                CancellationToken::new(),
            )
            .await
            .expect("confined command");
        let after = leveler_win_confine::integrity_label(workspace.path()).expect("label after");
        assert_eq!(before, after, "the write root's label was left behind");
    }

    /// The capability this backend exists for, end to end: a real compile,
    /// driven by the toolchain the host installed, writing only where the
    /// command was authorized to write. `cargo --version` proves the binary
    /// is readable; this proves the build it drives can actually run.
    #[tokio::test]
    async fn a_confined_command_builds_a_real_rust_package() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let runner = host_runner();
        let created = runner
            .run(
                confined(
                    "cargo init --bin --name leveler_confine_probe",
                    workspace.path(),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("cargo init must run");
        assert_eq!(created.exit_code, Some(0), "cargo init: {created:?}");

        let built = runner
            .run(
                confined("cargo build --offline", workspace.path()),
                CancellationToken::new(),
            )
            .await
            .expect("cargo build must run");
        assert_eq!(
            built.exit_code,
            Some(0),
            "a confined build must compile: {built:?}"
        );
        assert!(
            workspace.path().join("target").is_dir(),
            "the build must have written its artifacts into the workspace"
        );
    }

    /// The read authority comes from the execution environment, not from a
    /// list of toolchains this crate knows about. Git is the cheapest second
    /// witness: a different vendor, a different install location, no special
    /// case anywhere in the backend.
    #[tokio::test]
    async fn confinement_is_not_written_around_one_toolchain() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let runner = host_runner();
        let version = runner
            .run(
                confined("git --version", workspace.path()),
                CancellationToken::new(),
            )
            .await
            .expect("confined git must run");
        assert_eq!(version.exit_code, Some(0), "git --version: {version:?}");
        assert!(version.stdout.contains("git version"), "{version:?}");

        let initialized = runner
            .run(
                confined("git init", workspace.path()),
                CancellationToken::new(),
            )
            .await
            .expect("confined git init must run");
        assert_eq!(initialized.exit_code, Some(0), "git init: {initialized:?}");
        assert!(workspace.path().join(".git").is_dir());
    }

    /// Not a test of its own: the other half of the crash fixture below. With
    /// `LEVELER_CRASH_FIXTURE_ROOT` set it lowers that root and then blocks, so
    /// the parent can kill it between lowering the label and restoring it —
    /// which is the one state no `Drop` can clean up.
    #[test]
    fn windows_crash_fixture_child() {
        let Ok(root) = std::env::var(CRASH_FIXTURE_ROOT) else {
            return;
        };
        let environment = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ));
        let lease = super::lease_write_roots(&environment, &[PathBuf::from(root)])
            .expect("the fixture child must lower its root");
        // Hold it, and wait to be killed. If the parent somehow does not, the
        // sleep ends and the lease drops normally rather than hanging CI.
        std::thread::sleep(std::time::Duration::from_secs(120));
        drop(lease);
    }

    const CRASH_FIXTURE_ROOT: &str = "LEVELER_CRASH_FIXTURE_ROOT";
    const CRASH_FIXTURE_TEST: &str =
        "windows_confine::windows_canaries::windows_crash_fixture_child";

    /// The defect this closes: a run killed with a write root still lowered
    /// leaves the user's directory at Low integrity, and `Drop` never runs. A
    /// later run has to put it back without being told which root to look at.
    ///
    /// Nothing here is simulated. A real child process lowers a real label and
    /// is really killed.
    #[tokio::test]
    async fn a_killed_run_leaves_a_low_root_and_the_next_one_puts_it_back() {
        if !launcher_or_skip() {
            return;
        }
        // A private home, so the fixture's records cannot be confused with the
        // host's own and cannot outlive the test.
        let home = tempfile::tempdir().expect("home");
        let workspace = tempfile::tempdir().expect("workspace");
        let environment = leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("LEVELER_HOME"),
                std::ffi::OsString::from(home.path()),
            )],
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        );

        let before = leveler_win_confine::integrity_label(workspace.path()).expect("label before");
        assert!(
            !before.is_low(),
            "the fixture needs a root that is not already Low: {before:?}"
        );

        let mut child = std::process::Command::new(
            std::env::current_exe().expect("this test binary is the fixture child too"),
        )
        .args([
            "--exact",
            CRASH_FIXTURE_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LEVELER_HOME", home.path())
        .env(CRASH_FIXTURE_ROOT, workspace.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the fixture child");

        // Wait for the label to actually be down — the precondition, observed
        // rather than assumed.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if leveler_win_confine::integrity_label(workspace.path())
                .map(|label| label.is_low())
                .unwrap_or(false)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the fixture child never lowered the root"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // Kill it. No unwinding, no destructors, no restore.
        child.kill().expect("kill the fixture child");
        child.wait().expect("reap the fixture child");

        assert!(
            leveler_win_confine::integrity_label(workspace.path())
                .expect("label after the kill")
                .is_low(),
            "the killed run was supposed to leave the root Low — without that \
             there is nothing for recovery to prove"
        );
        let records = super::records_dir(&environment);
        let leftover: Vec<_> = std::fs::read_dir(&records)
            .expect("records dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "label")
            })
            .collect();
        assert_eq!(
            leftover.len(),
            1,
            "exactly one leftover record: {leftover:?}"
        );

        // The next run, with no idea which repository it is about to repair.
        super::recover_stale_write_roots(&environment).expect("recovery must succeed");

        let after = leveler_win_confine::integrity_label(workspace.path()).expect("label after");
        assert_eq!(after, before, "the root's original label must be back");
        assert!(
            !leftover[0].exists(),
            "a record whose root was restored must not survive it"
        );

        // And again: recovery has to be safe to repeat.
        super::recover_stale_write_roots(&environment).expect("recovery must be idempotent");
        assert_eq!(
            leveler_win_confine::integrity_label(workspace.path()).expect("label"),
            before
        );
    }

    /// The bug this rule exists for: a shell command whose own argument is
    /// quoted has to reach the program as the user wrote it. `cmd /C` parses
    /// its tail itself and does not read a backslash as a quote escape, so
    /// quoting that tail the Win32 way delivered the quotes as characters and
    /// PowerShell printed the command instead of running it. Confined and
    /// unconfined must agree, so both are checked here.
    #[tokio::test]
    async fn a_quoted_shell_argument_reaches_the_program_as_written() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let runner = host_runner();
        let line = "powershell -NoProfile -NonInteractive -Command \"Write-Output 'quoting-ok'\"";
        let (program, args) = crate::command::shell_invocation(line);

        for scope in [
            WriteScope::Workspace {
                root: workspace.path().to_path_buf(),
            },
            WriteScope::Unrestricted,
        ] {
            let mut request = ProcessRequest::new(
                program.clone(),
                args.clone(),
                workspace.path().to_path_buf(),
            );
            request.write_scope = scope.clone();
            let output = runner
                .run(request, CancellationToken::new())
                .await
                .expect("the shell command must run");
            assert_eq!(output.exit_code, Some(0), "{scope:?}: {output:?}");
            assert!(
                output.stdout.contains("quoting-ok"),
                "{scope:?}: the command must run, not be echoed: {output:?}"
            );
            assert!(
                !output.stdout.contains("Write-Output"),
                "{scope:?}: the command text leaked into its own output: {output:?}"
            );
        }
    }

    /// Background used to be the unconfined path. It is the same path now, so
    /// the same canary has to hold for it.
    #[tokio::test]
    async fn a_confined_background_command_is_confined_too() {
        if !launcher_or_skip() {
            return;
        }
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("escape.txt");
        let registry = host_registry();

        let id = registry
            .spawn(
                confined(
                    &format!("echo escaped> \"{}\"", target.display()),
                    workspace.path(),
                ),
                None,
            )
            .await
            .expect("a confined background command must spawn, not be refused");
        let snapshot = registry
            .wait(
                &id,
                Some(std::time::Duration::from_secs(30)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait");
        assert_ne!(snapshot.exit_code, Some(0), "{snapshot:?}");
        assert!(
            !target.exists(),
            "a confined background command wrote outside its workspace"
        );

        let inside_id = registry
            .spawn(confined("echo inside> inside.txt", workspace.path()), None)
            .await
            .expect("spawn inside");
        let inside = registry
            .wait(
                &inside_id,
                Some(std::time::Duration::from_secs(30)),
                &CancellationToken::new(),
            )
            .await
            .expect("wait inside");
        assert_eq!(inside.exit_code, Some(0), "{inside:?}");
        assert!(workspace.path().join("inside.txt").is_file());
        let _ = PathBuf::new();
    }
}
