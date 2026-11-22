//! The persisted trust records and the human-only approval transaction.
//!
//! The store lives in the CodeLeveler home, outside the workspace. On Windows
//! the home is Medium integrity while confined commands run Low; the sandbox's
//! no-write-up rule therefore denies create, write, rename and delete there.
//! On macOS/Linux the sandbox binds the home read-only and grants writes only
//! to the workspace and temp roots. A store the confined execution can write
//! is not a store, so an arrangement that puts the home inside the project is
//! refused outright.
//!
//! Updates take an exclusive lock and merge under it: two sessions approving
//! different definitions cannot lose one another's records. Writes are atomic
//! (temp file + rename), so a reader sees either the previous or the new store,
//! never a partial one. A corrupt or future-version store is a hard failure
//! for approvals and fails closed for reads.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::{
    DefinitionIdentity, DefinitionScope, STORE_FILE_NAME, STORE_VERSION, USER_DOMAIN,
    project_domain, store_is_outside_project,
};

/// How many bytes of an approved definition are kept for a later diff. Bodies
/// above this limit still get a trust record (the digest is the authority);
/// only the convenience snapshot is skipped.
pub const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;

/// Which human surface performed the approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalSurface {
    Cli,
    Tui,
    Startup,
}

/// The state of the on-disk store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreStatus {
    /// No store file. Nothing is trusted — this is the normal first run, not an
    /// error, and it never means "trust everything".
    Missing,
    Healthy,
    /// Present but unreadable, not valid YAML, or structurally invalid. Reads
    /// fail closed; approvals refuse rather than overwrite unknown records.
    Corrupt {
        detail: String,
    },
    /// Written by a newer CodeLeveler that knows fields this build does not.
    UnsupportedVersion {
        found: u32,
    },
    /// The CodeLeveler home resolves inside the project it would grant for.
    /// The project's sandboxed execution could rewrite such a store.
    InsideProject,
}

impl StoreStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Healthy => "healthy",
            Self::Corrupt { .. } => "corrupt",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::InsideProject => "store_inside_project",
        }
    }

    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Missing | Self::Healthy)
    }
}

/// One persisted approval. The digest is over the exact bytes the human saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionApprovalRecord {
    /// `sha256:<hex>` over the definition's content (composite for agents).
    pub digest: String,
    /// RFC 3339 UTC, host clock. Informational only; never a validity check.
    pub approved_at: String,
    pub approval: ApprovalSurface,
    /// Monotonic per-domain version assigned by this approval transaction.
    pub approval_version: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DomainRecord {
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    definitions: BTreeMap<String, DefinitionApprovalRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    #[serde(default)]
    domains: BTreeMap<String, DomainRecord>,
}

impl Default for StoreFile {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            domains: BTreeMap::new(),
        }
    }
}

/// A loaded store. `status` is part of the value: callers must not treat an
/// unreadable store as an empty one.
#[derive(Debug, Clone)]
pub struct DefinitionTrustStore {
    root: PathBuf,
    file: StoreFile,
    status: StoreStatus,
}

/// The result of a read, including the status that produced it.
#[derive(Debug, Clone)]
pub struct StoreLoad {
    pub status: StoreStatus,
}

#[derive(Debug, thiserror::Error)]
pub enum TrustStoreError {
    #[error("the definition trust store is unreadable or corrupt: {0}")]
    Corrupt(String),
    #[error("the definition trust store was written by a newer version (schema {0})")]
    UnsupportedVersion(u32),
    #[error(
        "the CodeLeveler home is inside this project, so the project could rewrite its own trust store"
    )]
    StoreInsideProject,
    #[error("the definition trust store could not be locked: {0}")]
    Lock(String),
    #[error("the definition trust store could not be written: {0}")]
    Io(#[from] std::io::Error),
    #[error("the approval did not name a project or user definition")]
    InvalidScope,
}

/// One definition the human approved, derived from the exact bytes read for
/// that decision. Constructors compute the digest, so a caller cannot record a
/// digest for bytes it did not hash.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    identity: DefinitionIdentity,
    digest: String,
    snapshot: Option<Vec<u8>>,
}

impl ApprovalRequest {
    /// A rule document.
    pub fn single(identity: DefinitionIdentity, bytes: &[u8]) -> Self {
        let digest = DefinitionIdentity::single_digest(bytes);
        let snapshot = (bytes.len() <= MAX_SNAPSHOT_BYTES).then(|| bytes.to_vec());
        Self {
            identity,
            digest,
            snapshot,
        }
    }

    /// A skill package: `SKILL.md` plus every approved reference file.
    ///
    /// The caller must pass the bytes it read for this decision; the digest
    /// binds package-relative names as well as contents, so swapping or
    /// renaming a reference is a different approval.
    pub fn skill_package(identity: DefinitionIdentity, parts: &[crate::SkillPackagePart]) -> Self {
        let digest = DefinitionIdentity::skill_package_digest(parts);
        let mut snapshot = Vec::new();
        let total: usize = parts.iter().map(|part| part.bytes.len()).sum();
        if total <= MAX_SNAPSHOT_BYTES {
            for part in parts {
                snapshot.extend_from_slice(format!("--- {} ---\n", part.relative).as_bytes());
                snapshot.extend_from_slice(&part.bytes);
                if !part.bytes.ends_with(b"\n") {
                    snapshot.push(b'\n');
                }
            }
        }
        Self {
            identity,
            digest,
            snapshot: (!snapshot.is_empty()).then_some(snapshot),
        }
    }

    /// Both files of a declarative agent.
    pub fn agent(identity: DefinitionIdentity, manifest: &[u8], instructions: &[u8]) -> Self {
        let digest = DefinitionIdentity::agent_digest(manifest, instructions);
        let mut snapshot = Vec::new();
        if manifest.len() + instructions.len() <= MAX_SNAPSHOT_BYTES {
            snapshot.extend_from_slice(b"--- agent.yaml ---\n");
            snapshot.extend_from_slice(manifest);
            if !manifest.ends_with(b"\n") {
                snapshot.push(b'\n');
            }
            snapshot.extend_from_slice(b"--- instructions.md ---\n");
            snapshot.extend_from_slice(instructions);
            if !instructions.ends_with(b"\n") {
                snapshot.push(b'\n');
            }
        }
        Self {
            identity,
            digest,
            snapshot: (!snapshot.is_empty()).then_some(snapshot),
        }
    }

    pub fn identity(&self) -> &DefinitionIdentity {
        &self.identity
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// What an approval transaction did.
#[derive(Debug, Clone, Default)]
pub struct ApprovalReport {
    pub approved: Vec<ApprovedDefinition>,
    /// The per-domain approval version assigned to this transaction.
    pub approval_version: u64,
}

#[derive(Debug, Clone)]
pub struct ApprovedDefinition {
    pub identity: DefinitionIdentity,
    pub digest: String,
    pub previous_digest: Option<String>,
}

/// `<home>/definition-trust.yaml`.
pub fn store_path(store_root: &Path) -> PathBuf {
    store_root.join(STORE_FILE_NAME)
}

/// The directory that holds approved-content snapshots. The whole directory is
/// a security-control namespace: no file under it may be authored by a tool.
pub fn snapshot_root(store_root: &Path) -> PathBuf {
    store_root.join("definition-trust")
}

fn snapshot_dir(store_root: &Path) -> PathBuf {
    snapshot_root(store_root).join("snapshots")
}

pub fn lock_path(store_root: &Path) -> PathBuf {
    store_root.join("definition-trust.lock")
}

impl DefinitionTrustStore {
    /// A store that trusts nothing. Used when no project/home is known and in
    /// tests that exercise built-ins or denial; never a way to grant trust.
    pub fn empty() -> Self {
        Self {
            root: PathBuf::new(),
            file: StoreFile::default(),
            status: StoreStatus::Missing,
        }
    }

    /// Load the store. Every failure mode yields a status that trusts nothing.
    pub fn load(store_root: &Path) -> Self {
        let path = store_path(store_root);
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    root: store_root.to_path_buf(),
                    file: StoreFile::default(),
                    status: StoreStatus::Missing,
                };
            }
            Err(error) => {
                return Self {
                    root: store_root.to_path_buf(),
                    file: StoreFile::default(),
                    status: StoreStatus::Corrupt {
                        detail: format!("cannot read {}: {error}", path.display()),
                    },
                };
            }
        };
        if raw.iter().all(u8::is_ascii_whitespace) {
            // An empty file is not a valid store (and not a reason to trust
            // anything); treat it exactly like a missing one.
            return Self {
                root: store_root.to_path_buf(),
                file: StoreFile::default(),
                status: StoreStatus::Missing,
            };
        }
        let file: StoreFile = match serde_yaml::from_slice(&raw) {
            Ok(file) => file,
            Err(error) => {
                return Self {
                    root: store_root.to_path_buf(),
                    file: StoreFile::default(),
                    status: StoreStatus::Corrupt {
                        detail: format!("invalid definition trust store: {error}"),
                    },
                };
            }
        };
        if file.version != STORE_VERSION {
            return Self {
                root: store_root.to_path_buf(),
                file: StoreFile::default(),
                status: StoreStatus::UnsupportedVersion {
                    found: file.version,
                },
            };
        }
        if let Some((domain, key)) = invalid_record(&file) {
            return Self {
                root: store_root.to_path_buf(),
                file: StoreFile::default(),
                status: StoreStatus::Corrupt {
                    detail: format!("invalid digest record `{key}` in domain `{domain}`"),
                },
            };
        }
        Self {
            root: store_root.to_path_buf(),
            file,
            status: StoreStatus::Healthy,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn status(&self) -> &StoreStatus {
        &self.status
    }

    pub fn path(&self) -> PathBuf {
        store_path(&self.root)
    }

    /// The exact approved digest for `identity` in `domain`.
    pub fn approved_digest(&self, domain: &str, identity: &DefinitionIdentity) -> Option<&str> {
        self.file
            .domains
            .get(domain)
            .and_then(|record| record.definitions.get(&identity.id))
            .map(|record| record.digest.as_str())
    }

    pub fn approval_record(
        &self,
        domain: &str,
        identity: &DefinitionIdentity,
    ) -> Option<&DefinitionApprovalRecord> {
        self.file
            .domains
            .get(domain)
            .and_then(|record| record.definitions.get(&identity.id))
    }

    pub fn domain_generation(&self, domain: &str) -> u64 {
        self.file
            .domains
            .get(domain)
            .map_or(0, |record| record.generation)
    }

    /// Every approved identity in the project domain, sorted by id.
    pub fn project_approvals(
        &self,
        project_root: &Path,
    ) -> impl Iterator<Item = (&String, &DefinitionApprovalRecord)> {
        let domain = project_domain(project_root);
        self.file
            .domains
            .get(&domain)
            .into_iter()
            .flat_map(|record| record.definitions.iter())
    }

    /// Every approved user-scope identity, sorted by id.
    pub fn user_approvals(&self) -> impl Iterator<Item = (&String, &DefinitionApprovalRecord)> {
        self.file
            .domains
            .get(USER_DOMAIN)
            .into_iter()
            .flat_map(|record| record.definitions.iter())
    }
}

fn invalid_record(file: &StoreFile) -> Option<(String, String)> {
    for (domain, record) in &file.domains {
        for (key, approval) in &record.definitions {
            let invalid = key.trim().is_empty()
                || match approval.digest.strip_prefix("sha256:") {
                    Some(digest) => {
                        digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    }
                    None => true,
                };
            if invalid {
                return Some((domain.clone(), key.clone()));
            }
        }
    }
    None
}

/// Approve exact definition contents. The bytes are read by the caller; these
/// constructors hash exactly the bytes handed in. The digest the caller shows
/// the human must be re-verified by the caller before building a request.
pub fn approve(
    store_root: &Path,
    project_root: &Path,
    requests: &[ApprovalRequest],
    surface: ApprovalSurface,
) -> Result<ApprovalReport, TrustStoreError> {
    if requests.is_empty() {
        return Ok(ApprovalReport::default());
    }
    if !store_is_outside_project(store_root, project_root) {
        return Err(TrustStoreError::StoreInsideProject);
    }
    let _lock = StoreLock::acquire(store_root)?;
    let loaded = DefinitionTrustStore::load(store_root);
    match &loaded.status {
        StoreStatus::Healthy | StoreStatus::Missing => {}
        StoreStatus::Corrupt { detail } => {
            return Err(TrustStoreError::Corrupt(detail.clone()));
        }
        StoreStatus::UnsupportedVersion { found } => {
            return Err(TrustStoreError::UnsupportedVersion(*found));
        }
        StoreStatus::InsideProject => return Err(TrustStoreError::StoreInsideProject),
    }
    let mut file = loaded.file;
    let project_key = project_domain(project_root);
    let mut report = ApprovalReport::default();
    // Assign one version per domain touched by this transaction.
    for request in requests {
        let domain = match request.identity.scope {
            DefinitionScope::Project => project_key.clone(),
            DefinitionScope::User => USER_DOMAIN.to_string(),
            DefinitionScope::Builtin => return Err(TrustStoreError::InvalidScope),
        };
        let record = file.domains.entry(domain.clone()).or_default();
        record.generation = record.generation.saturating_add(1);
        let version = record.generation;
        let previous = record.definitions.get(&request.identity.id).cloned();
        record.definitions.insert(
            request.identity.id.clone(),
            DefinitionApprovalRecord {
                digest: request.digest.clone(),
                approved_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                approval: surface,
                approval_version: version,
            },
        );
        report.approval_version = report.approval_version.max(version);
        report.approved.push(ApprovedDefinition {
            identity: request.identity.clone(),
            digest: request.digest.clone(),
            previous_digest: previous.map(|record| record.digest),
        });
        if let Some(snapshot) = &request.snapshot {
            write_snapshot(store_root, &request.digest, snapshot)?;
        }
    }
    write_store(store_root, &file)?;
    Ok(report)
}

/// Drop every project-scope approval for `project_root`.
pub fn revoke_all(store_root: &Path, project_root: &Path) -> Result<bool, TrustStoreError> {
    if !store_is_outside_project(store_root, project_root) {
        return Err(TrustStoreError::StoreInsideProject);
    }
    let _lock = StoreLock::acquire(store_root)?;
    let loaded = DefinitionTrustStore::load(store_root);
    match &loaded.status {
        StoreStatus::Healthy | StoreStatus::Missing => {}
        StoreStatus::Corrupt { detail } => {
            return Err(TrustStoreError::Corrupt(detail.clone()));
        }
        StoreStatus::UnsupportedVersion { found } => {
            return Err(TrustStoreError::UnsupportedVersion(*found));
        }
        StoreStatus::InsideProject => return Err(TrustStoreError::StoreInsideProject),
    }
    let mut file = loaded.file;
    let key = project_domain(project_root);
    let removed = file.domains.remove(&key).is_some();
    if removed {
        write_store(store_root, &file)?;
    }
    Ok(removed)
}

/// Drop one approval. The identity may be project or user scope; the project
/// root is still required for the outside-the-project check.
pub fn revoke_definition(
    store_root: &Path,
    project_root: &Path,
    identity: &DefinitionIdentity,
) -> Result<bool, TrustStoreError> {
    if !store_is_outside_project(store_root, project_root) {
        return Err(TrustStoreError::StoreInsideProject);
    }
    let _lock = StoreLock::acquire(store_root)?;
    let loaded = DefinitionTrustStore::load(store_root);
    match &loaded.status {
        StoreStatus::Healthy | StoreStatus::Missing => {}
        StoreStatus::Corrupt { detail } => {
            return Err(TrustStoreError::Corrupt(detail.clone()));
        }
        StoreStatus::UnsupportedVersion { found } => {
            return Err(TrustStoreError::UnsupportedVersion(*found));
        }
        StoreStatus::InsideProject => return Err(TrustStoreError::StoreInsideProject),
    }
    let mut file = loaded.file;
    let domain = match identity.scope {
        DefinitionScope::Project => project_domain(project_root),
        DefinitionScope::User => USER_DOMAIN.to_string(),
        DefinitionScope::Builtin => return Err(TrustStoreError::InvalidScope),
    };
    let removed = file.domains.get_mut(&domain).is_some_and(|record| {
        let removed = record.definitions.remove(&identity.id).is_some();
        if removed {
            record.generation = record.generation.saturating_add(1);
        }
        removed
    });
    if removed {
        write_store(store_root, &file)?;
    }
    Ok(removed)
}

/// Read the approved-content snapshot for a digest, when one was small enough
/// to keep. Used for a diff; the trust decision never depends on it.
pub fn read_snapshot(store_root: &Path, digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    std::fs::read(snapshot_dir(store_root).join(hex)).ok()
}

fn write_snapshot(store_root: &Path, digest: &str, bytes: &[u8]) -> Result<(), TrustStoreError> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Ok(());
    };
    let dir = snapshot_dir(store_root);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(hex);
    if path.is_file() {
        return Ok(());
    }
    write_private_file(&path, bytes)
}

fn write_store(store_root: &Path, file: &StoreFile) -> Result<(), TrustStoreError> {
    std::fs::create_dir_all(store_root)?;
    let yaml = serde_yaml::to_string(file)
        .map_err(|error| std::io::Error::other(format!("serialize trust store: {error}")))?;
    let target = store_path(store_root);
    let temp = store_root.join(format!(
        ".{STORE_FILE_NAME}.tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos())
    ));
    {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut handle = options.open(&temp)?;
        handle.write_all(yaml.as_bytes())?;
        handle.sync_all()?;
    }
    // `rename` replaces the destination on Windows and POSIX. If it fails, the
    // temp file is removed so a later run does not see a stray approval-looking
    // file (the name starts with `.` and is never read as the store).
    if let Err(error) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(TrustStoreError::Io(error));
    }
    #[cfg(unix)]
    {
        if let Ok(dir) = File::open(store_root) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), TrustStoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut handle = options.open(path)?;
    handle.write_all(bytes)?;
    handle.sync_all()?;
    Ok(())
}

/// An exclusive lock scoped to one approval transaction.
struct StoreLock {
    file: File,
}

impl StoreLock {
    fn acquire(store_root: &Path) -> Result<Self, TrustStoreError> {
        std::fs::create_dir_all(store_root)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(lock_path(store_root))?;
        file.lock_exclusive()
            .map_err(|error| TrustStoreError::Lock(error.to_string()))?;
        Ok(Self { file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefinitionIdentity, DefinitionKind};
    use tempfile::tempdir;

    fn identity() -> DefinitionIdentity {
        DefinitionIdentity::project(DefinitionKind::RuleFile, ".leveler/rules/rule.md").unwrap()
    }

    fn request(body: &str) -> ApprovalRequest {
        ApprovalRequest::single(identity(), body.as_bytes())
    }

    #[test]
    fn a_missing_store_is_not_an_error_and_trusts_nothing() {
        let home = tempdir().unwrap();
        let store = DefinitionTrustStore::load(home.path());
        assert_eq!(store.status(), &StoreStatus::Missing);
        assert_eq!(store.approved_digest("x", &identity()), None);
    }

    #[test]
    fn approval_records_the_exact_digest_and_survives_a_reload() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let report = approve(
            home.path(),
            project.path(),
            &[request("RULE-A")],
            ApprovalSurface::Cli,
        )
        .unwrap();
        assert_eq!(report.approved.len(), 1);
        let store = DefinitionTrustStore::load(home.path());
        assert_eq!(store.status(), &StoreStatus::Healthy);
        let digest = store
            .approved_digest(&project_domain(project.path()), &identity())
            .unwrap();
        assert_eq!(digest, DefinitionIdentity::single_digest(b"RULE-A"));
        assert_eq!(
            read_snapshot(home.path(), digest).as_deref(),
            Some(b"RULE-A".as_slice())
        );
    }

    #[test]
    fn a_corrupt_store_fails_closed_and_refuses_approval() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        std::fs::write(store_path(home.path()), "{not: [valid").unwrap();
        let store = DefinitionTrustStore::load(home.path());
        assert!(matches!(store.status(), StoreStatus::Corrupt { .. }));
        assert!(matches!(
            approve(
                home.path(),
                project.path(),
                &[request("RULE-A")],
                ApprovalSurface::Cli
            ),
            Err(TrustStoreError::Corrupt(_))
        ));
        // The corrupt bytes were not overwritten by the refused transaction.
        assert_eq!(
            std::fs::read_to_string(store_path(home.path())).unwrap(),
            "{not: [valid"
        );
    }

    #[test]
    fn a_newer_schema_is_refused_not_reinterpreted() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        std::fs::write(store_path(home.path()), "version: 99\ndomains: {}\n").unwrap();
        assert_eq!(
            DefinitionTrustStore::load(home.path()).status(),
            &StoreStatus::UnsupportedVersion { found: 99 }
        );
        assert!(matches!(
            approve(
                home.path(),
                project.path(),
                &[request("RULE-A")],
                ApprovalSurface::Cli
            ),
            Err(TrustStoreError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn a_store_inside_the_project_grants_nothing() {
        let project = tempdir().unwrap();
        let home = project.path().join(".leveler-home");
        std::fs::create_dir_all(&home).unwrap();
        assert!(matches!(
            approve(
                &home,
                project.path(),
                &[request("RULE-A")],
                ApprovalSurface::Cli
            ),
            Err(TrustStoreError::StoreInsideProject)
        ));
    }

    #[test]
    fn concurrent_approvals_merge_under_the_lock() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let other =
            DefinitionIdentity::project(DefinitionKind::RuleFile, ".leveler/rules/other.md")
                .unwrap();
        approve(
            home.path(),
            project.path(),
            &[request("ROOT")],
            ApprovalSurface::Cli,
        )
        .unwrap();
        approve(
            home.path(),
            project.path(),
            &[ApprovalRequest::single(other.clone(), b"NESTED")],
            ApprovalSurface::Cli,
        )
        .unwrap();
        let store = DefinitionTrustStore::load(home.path());
        let domain = project_domain(project.path());
        assert!(store.approved_digest(&domain, &identity()).is_some());
        assert!(store.approved_digest(&domain, &other).is_some());
        assert!(store.domain_generation(&domain) >= 2);
    }

    #[test]
    fn concurrent_approvals_from_separate_threads_merge() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let home_path = home.path().to_path_buf();
        let project_path = project.path().to_path_buf();
        let mut handles = Vec::new();
        for index in 0..4 {
            let home = home_path.clone();
            let project = project_path.clone();
            handles.push(std::thread::spawn(move || {
                let identity = DefinitionIdentity::project(
                    DefinitionKind::RuleFile,
                    &format!(".leveler/rules/{index}.md"),
                )
                .unwrap();
                approve(
                    &home,
                    &project,
                    &[ApprovalRequest::single(
                        identity,
                        format!("RULE-{index}").as_bytes(),
                    )],
                    ApprovalSurface::Cli,
                )
                .unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let store = DefinitionTrustStore::load(&home_path);
        let domain = project_domain(&project_path);
        for index in 0..4 {
            let identity = DefinitionIdentity::project(
                DefinitionKind::RuleFile,
                &format!(".leveler/rules/{index}.md"),
            )
            .unwrap();
            assert!(
                store.approved_digest(&domain, &identity).is_some(),
                "lost approval {index}"
            );
        }
        // Each transaction increments the domain generation; none rolled back.
        assert!(store.domain_generation(&domain) >= 4);
    }

    #[test]
    fn a_record_with_a_malformed_digest_is_corrupt_not_trusted() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let identity = identity();
        std::fs::write(
            store_path(home.path()),
            format!(
                "version: 1
domains:
  \"{}\":
    generation: 1
    definitions:
      \"{}\":
        digest: not-a-digest
        approved_at: x
        approval: cli
        approval_version: 1
",
                project_domain(project.path()),
                identity.id
            ),
        )
        .unwrap();
        let store = DefinitionTrustStore::load(home.path());
        assert!(matches!(store.status(), StoreStatus::Corrupt { .. }));
        let view = crate::DefinitionTrustView::load(home.path(), Some(project.path()));
        assert!(matches!(
            view.check(&identity, &DefinitionIdentity::single_digest(b"x")),
            crate::DefinitionTrust::StoreUnavailable { .. }
        ));
    }

    #[test]
    fn revoking_one_definition_leaves_the_others() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let other =
            DefinitionIdentity::project(DefinitionKind::Skill, ".leveler/skills/foo").unwrap();
        approve(
            home.path(),
            project.path(),
            &[
                request("ROOT"),
                ApprovalRequest::single(other.clone(), b"SKILL"),
            ],
            ApprovalSurface::Cli,
        )
        .unwrap();
        assert!(revoke_definition(home.path(), project.path(), &identity()).unwrap());
        let store = DefinitionTrustStore::load(home.path());
        let domain = project_domain(project.path());
        assert!(store.approved_digest(&domain, &identity()).is_none());
        assert!(store.approved_digest(&domain, &other).is_some());
        assert!(!revoke_definition(home.path(), project.path(), &identity()).unwrap());
    }

    #[test]
    fn a_snapshot_is_keyed_by_digest_and_path_traversal_is_refused() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let hello = DefinitionIdentity::single_digest(b"hello");
        approve(
            home.path(),
            project.path(),
            &[ApprovalRequest::single(identity(), b"hello")],
            ApprovalSurface::Cli,
        )
        .unwrap();
        assert_eq!(
            read_snapshot(home.path(), &hello).as_deref(),
            Some(b"hello".as_slice())
        );
        assert!(read_snapshot(home.path(), "../../etc/passwd").is_none());
        assert!(read_snapshot(home.path(), "sha256:../../etc/passwd").is_none());
    }
}
