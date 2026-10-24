//! Shared fixtures for the Memory 2.0 Stage 1 tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use leveler_memory::v2::{
    Admission, AdmitRequest, Caller, Experience, ExperienceEvidence, MarkdownStore, MemoryIntent,
    MemoryKind, MemoryScope, MemoryStatus, NewRecord, OpId, Relation, ScopeRoots, SourceRef,
    StatementKind,
};
use tempfile::TempDir;
use uuid::Uuid;

pub struct Fixture {
    dir: TempDir,
    pub current: PathBuf,
    pub state: PathBuf,
    pub repo: Option<PathBuf>,
    pub scope: MemoryScope,
}

impl Fixture {
    pub fn new(scope: MemoryScope) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (current, state, repo) = match scope {
            MemoryScope::Project | MemoryScope::Global => {
                (dir.path().join("current"), dir.path().join("state"), None)
            }
            MemoryScope::Local => {
                let repo = dir.path().join("repo");
                std::fs::create_dir_all(&repo).unwrap();
                let status = Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .arg("init")
                    .arg("-q")
                    .status()
                    .expect("git is required for local-scope tests");
                assert!(status.success(), "git init failed");
                (
                    repo.join(".leveler").join("local"),
                    dir.path().join("state"),
                    Some(repo),
                )
            }
        };
        Self {
            dir,
            current,
            state,
            repo,
            scope,
        }
    }

    pub fn roots(&self) -> ScopeRoots {
        let roots = ScopeRoots::new(self.scope, &self.current, &self.state);
        match &self.repo {
            Some(repo) => roots.with_repo_root(repo),
            None => roots,
        }
    }

    pub fn store(&self) -> MarkdownStore {
        MarkdownStore::open(self.roots()).unwrap()
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub fn current_file(&self, id: &leveler_memory::v2::MemoryId) -> PathBuf {
        self.current.join(id.file_name())
    }
}

/// A mounted disk image, used to place the current root and the private state
/// root on genuinely different filesystems. `None` means this host cannot
/// mount one; callers must report that as unverified rather than pass.
#[cfg(target_os = "macos")]
pub struct MountedVolume {
    mount: PathBuf,
    image: PathBuf,
}

#[cfg(target_os = "macos")]
impl MountedVolume {
    pub fn new(label: &str) -> Option<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let volume = format!("LevelerMemory{unique}");
        let image = std::env::temp_dir().join(format!("leveler-memory-{label}-{unique}.dmg"));
        let created = Command::new("hdiutil")
            .args(["create", "-size", "32m", "-fs", "HFS+", "-volname"])
            .arg(&volume)
            .arg("-quiet")
            .arg(&image)
            .status()
            .ok()?;
        if !created.success() {
            let _ = std::fs::remove_file(&image);
            return None;
        }
        let attached = Command::new("hdiutil")
            .args(["attach", "-nobrowse"])
            .arg(&image)
            .output()
            .ok()?;
        if !attached.status.success() {
            let _ = std::fs::remove_file(&image);
            return None;
        }
        let stdout = String::from_utf8_lossy(&attached.stdout);
        let mount = stdout
            .lines()
            .find(|line| line.contains("/Volumes/"))
            .and_then(|line| line.split_whitespace().last())
            .map(PathBuf::from)?;
        Some(Self { mount, image })
    }

    pub fn path(&self) -> &Path {
        &self.mount
    }
}

#[cfg(target_os = "macos")]
impl Drop for MountedVolume {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .arg("detach")
            .arg(&self.mount)
            .arg("-quiet")
            .status();
        let _ = std::fs::remove_file(&self.image);
    }
}

pub fn op() -> OpId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    OpId::parse(format!(
        "op-{}-{sequence}-{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ))
    .unwrap()
}

pub fn user() -> Caller {
    Caller::UserCommand {
        invocation: format!("invocation-{}", Uuid::new_v4().simple()),
    }
}

pub fn user_review() -> Caller {
    Caller::UserReview {
        invocation: format!("review-{}", Uuid::new_v4().simple()),
    }
}

pub fn model() -> Caller {
    Caller::Model {
        tool_call: format!("call-{}", Uuid::new_v4().simple()),
    }
}

pub fn runtime() -> Caller {
    Caller::Runtime {
        task: format!("task-{}", Uuid::new_v4().simple()),
    }
}

pub fn migration() -> Caller {
    Caller::Migration {
        run: format!("run-{}", Uuid::new_v4().simple()),
    }
}

pub fn fact_intent(
    scope: MemoryScope,
    subject_key: Option<&str>,
    value: &str,
    body: &str,
) -> MemoryIntent {
    MemoryIntent {
        kind: MemoryKind::UserFact,
        scope,
        subject: subject_key.map(str::to_string),
        subject_key: subject_key.map(str::to_string),
        value: Some(value.to_string()),
        title: subject_key.unwrap_or("memory").to_string(),
        body: body.to_string(),
        tags: Vec::new(),
        statement: StatementKind::Assertion,
        relation: Relation::None,
        expires_at: None,
        evidence: Vec::new(),
        experience: None,
        proposed_authority: leveler_memory::v2::MemoryAuthority::ModelInferred,
    }
}

pub fn experience_intent(
    scope: MemoryScope,
    category: &str,
    applies_when: &str,
    body: &str,
    resolved: bool,
) -> MemoryIntent {
    let mut evidence = vec![ExperienceEvidence {
        kind: "failure".into(),
        reference: "failure-1".into(),
    }];
    if resolved {
        evidence.push(ExperienceEvidence {
            kind: "resolution".into(),
            reference: "resolution-1".into(),
        });
    }
    MemoryIntent {
        kind: MemoryKind::Experience,
        scope,
        subject: None,
        subject_key: None,
        value: None,
        title: category.to_string(),
        body: body.to_string(),
        tags: Vec::new(),
        statement: StatementKind::Note,
        relation: Relation::None,
        expires_at: None,
        evidence: Vec::new(),
        experience: Some(Experience {
            category: category.to_string(),
            applies_when: applies_when.to_string(),
            invalid_if: None,
            environment: Default::default(),
            evidence,
        }),
        proposed_authority: leveler_memory::v2::MemoryAuthority::ModelInferred,
    }
}

pub fn admit(store: &MarkdownStore, caller: Caller, intent: MemoryIntent) -> Admission {
    store
        .admit(AdmitRequest {
            op_id: op(),
            caller,
            intent,
            expected: None,
        })
        .unwrap()
}

pub fn create_user(
    store: &MarkdownStore,
    scope: MemoryScope,
    key: &str,
    value: &str,
    body: &str,
) -> Admission {
    admit(store, user(), fact_intent(scope, Some(key), value, body))
}

pub fn create_model(
    store: &MarkdownStore,
    scope: MemoryScope,
    key: &str,
    value: &str,
    body: &str,
) -> Admission {
    admit(store, model(), fact_intent(scope, Some(key), value, body))
}

pub fn applied_id(admission: &Admission) -> leveler_memory::v2::MemoryId {
    match admission {
        Admission::Applied { id, .. } | Admission::Replayed { id, .. } => id.clone(),
        other => panic!("expected an applied admission, got {other:?}"),
    }
}

pub fn pending_id(admission: &Admission) -> leveler_memory::v2::MemoryId {
    match admission {
        Admission::Pending { id, .. } => id.clone(),
        other => panic!("expected a pending admission, got {other:?}"),
    }
}

pub fn active_summaries(store: &MarkdownStore) -> Vec<leveler_memory::v2::MemorySummary> {
    store
        .list(leveler_memory::v2::ListQuery {
            scope: None,
            kind: None,
            status: Some(MemoryStatus::Active),
            limit: None,
            include_external: true,
        })
        .unwrap()
}

pub fn effective(store: &MarkdownStore) -> Vec<leveler_memory::v2::EffectiveRecord> {
    store.effective_active().unwrap()
}

pub fn new_record_for(intent: &MemoryIntent) -> NewRecord {
    NewRecord {
        kind: intent.kind,
        scope: intent.scope,
        subject: intent.subject.clone(),
        subject_key: intent.subject_key.clone(),
        value: intent.value.clone(),
        title: intent.title.clone(),
        body: intent.body.clone(),
        tags: intent.tags.clone(),
        statement: intent.statement,
        expires_at: intent.expires_at,
        evidence: intent.evidence.clone(),
        experience: intent.experience.clone(),
    }
}

/// A user-turn source whose span is exactly `body` — the provenance shape a
/// verified automatic user fact carries. A fixture that wants the user's
/// authority has to provide the user's words.
pub fn verbatim_source(body: &str) -> SourceRef {
    let mut source = source_ref();
    source.evidence_span = Some(body.to_string());
    source
}

pub fn source_ref() -> SourceRef {
    SourceRef {
        kind: leveler_memory::v2::SourceRefKind::UserTurn,
        session_id: Some("session-1".into()),
        turn_id: Some("turn-1".into()),
        tool_call_id: None,
        task_id: None,
        pending_id: None,
        evidence_span: Some("remember this".into()),
        message_digest: None,
        span_range: None,
        file_sha256: None,
        legacy_store: None,
        legacy_id: None,
        legacy_sha256: None,
        extra: Default::default(),
    }
}
