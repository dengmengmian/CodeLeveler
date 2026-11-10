//! Model-free execution substrate. Clients may disappear; this process keeps
//! draining output and owns every child from spawn through process-group exit.
//! Mutation accounting and agent/session decisions stay in the client runtime.

use crate::{
    BackgroundTaskLifetime, BackgroundTaskObservation, BackgroundTaskRegistry,
    BackgroundTaskSnapshot, BackgroundTaskStatus, ProcessRequest, WriteScope,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

pub const PROTOCOL_MAJOR: u32 = 2;
pub const PROTOCOL_MINOR: u32 = 2;
/// Older majors this build can still talk to.
///
/// The Execution Host is a persistent owner: a runtime upgrade must not strand
/// a service that an earlier host is still running. Major 1 speaks the same
/// owner-scoped control plane (`list`/`get`/`observe`/`stop`/`acknowledge`); it
/// predates the major-2 `unrestricted` control flag, which requires an explicit
/// capability this build refuses to assume. Everything else is unchanged, so a
/// legacy host keeps managing its own tasks and exits on its own once drained.
pub const SUPPORTED_LEGACY_MAJORS: &[u32] = &[1];
pub const REQUIRED_CAPABILITIES: &[&str] = &[
    "process.spawn",
    "process.list",
    "process.inspect",
    "process.stop",
    "logs.read",
];
/// Optional capability: a client may durably record that the runtime
/// generation which admitted a goal task is gone.
///
/// The host never signals or forgets the process: it keeps owning, draining
/// and reporting it. The record only states that no result can be delivered to
/// the generation that asked for the work. Clients must check this capability
/// before sending [`Request::OwnerLost`]; a host that predates it has no way to
/// represent the fact and keeps its previous behaviour.
pub const OWNER_LOSS_CAPABILITY: &str = "process.owner-loss";
const FRAME_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct ExecutionHostConfig {
    pub state_dir: PathBuf,
    pub repo_root: PathBuf,
    pub executable: PathBuf,
}
impl ExecutionHostConfig {
    pub fn semantic_dir(&self) -> PathBuf {
        self.state_dir.with_file_name("execution-settlement")
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostedTask {
    pub snapshot: BackgroundTaskSnapshot,
    pub lifetime: BackgroundTaskLifetime,
    pub writer_scope: String,
    pub write_scope: WriteScope,
    pub process_id: Option<u32>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub log_end: u64,
    pub log_prefix_len: usize,
    /// The runtime boot that admitted this task, when the spawn recorded it.
    ///
    /// A goal task belongs to the runtime generation that asked for it. This
    /// attribution is what lets a REPLACEMENT generation prove that the owner
    /// is gone instead of silently adopting the task as its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_boot: Option<String>,
    /// Proven loss of the runtime generation that admitted a goal task.
    ///
    /// This is not a terminal state: the host still owns and drains the
    /// process. It records that the result can no longer be delivered to the
    /// generation that admitted the work, so a replacement must not present
    /// the task as its own Running work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_loss: Option<OwnerLoss>,
    /// Set only by recovery, after the owning host was proven gone. Its
    /// presence means the exit fact is missing: the status is `Unknown` and
    /// [`TaskLoss::process`] says whether a workload may still exist.
    #[serde(default)]
    pub loss: Option<TaskLoss>,
    /// The committed completion record for a terminal task this host observed.
    ///
    /// Written together with the terminal snapshot in one atomic archive
    /// replacement; absent for a task whose exit the host never observed. A
    /// record is never synthesized from a `Unknown` convergence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<CompletionRecord>,
}

/// The durable identity and bounded result of one observed exit.
///
/// The host, not the runtime, owns this fact: it is produced from the child
/// handle the host waited on, and committed to the same atomic archive as the
/// snapshot. The digest makes repeated reads and repeated recoveries comparable
/// without trusting a mutable log field.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionRecord {
    /// Stable digest of the terminal fact. Identical across re-reads, so a
    /// repeated observation can prove it is the same result and not a new one.
    pub result_id: String,
    pub status: BackgroundTaskStatus,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    /// Bytes retained in the durable log at commit.
    pub output_bytes: u64,
    /// Whether earlier output was dropped by the bounded log.
    pub output_truncated: bool,
    pub completed_at_ms: u64,
}

impl CompletionRecord {
    fn of(
        task_id: &str,
        snapshot: &BackgroundTaskSnapshot,
        completed_at_ms: u64,
    ) -> CompletionRecord {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(task_id.as_bytes());
        hasher.update([0]);
        hasher.update(format!("{:?}", snapshot.status).as_bytes());
        hasher.update([0]);
        hasher.update(format!("{:?}", snapshot.exit_code).as_bytes());
        hasher.update([0]);
        hasher.update(format!("{:?}", snapshot.signal).as_bytes());
        hasher.update([0]);
        hasher.update(snapshot.log.as_bytes());
        hasher.update([0]);
        hasher.update(completed_at_ms.to_string().as_bytes());
        CompletionRecord {
            result_id: hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            status: snapshot.status,
            exit_code: snapshot.exit_code,
            signal: snapshot.signal,
            output_bytes: snapshot.log.len() as u64,
            output_truncated: snapshot.log_truncated,
            completed_at_ms,
        }
    }
}

impl HostedTask {
    /// The record still describes work its host owns. Recovery converges these;
    /// nothing else rewrites them.
    pub fn is_live(&self) -> bool {
        matches!(
            self.snapshot.status,
            BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
        )
    }

    /// Whether an unrecovered record may still describe a running workload.
    /// Only a recovery pass that found the process identity gone says no.
    pub fn process_may_still_exist(&self) -> bool {
        self.loss
            .as_ref()
            .is_some_and(|loss| loss.process.may_still_own_process())
    }

    /// Terminal status plus proof that nothing of this workload is left. This is
    /// the precondition for semantic settlement of its write reservation.
    ///
    /// `Unknown` alone is not proof: it means the owner lost the ability to
    /// observe the process. Only recovery's `Gone` evidence (or the host's own
    /// `Exited`/`Killed` fact) says the namespace can no longer be written.
    pub fn settlement_proven(&self) -> bool {
        match self.snapshot.status {
            BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed => true,
            BackgroundTaskStatus::Unknown => self
                .loss
                .as_ref()
                .is_some_and(|loss| !loss.process.may_still_own_process()),
            BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing => false,
        }
    }
}

/// What a recovery pass could observe about the process a lost host owned.
///
/// A lost host has no handle, pipes or child status any more; these are the only
/// OS facts left. They are never used to signal, adopt or replay a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessPresence {
    /// The recorded process group and its session have no members left: the
    /// workload is over. No exit code was observed, so the status is `Unknown`.
    Gone,
    /// A process with the recorded group or session identity still exists. A
    /// recycled id cannot be told apart from the lost workload, so it is
    /// neither signalled nor adopted.
    Present,
    /// No identity was recorded, or this platform cannot probe it.
    Unobservable,
}

impl ProcessPresence {
    /// Whether this evidence allows a write reservation to be settled. Only
    /// `Gone` proves that a workload can no longer write.
    pub fn may_still_own_process(self) -> bool {
        !matches!(self, Self::Gone)
    }
}

/// The durable explanation for a terminal task whose exit fact never arrived.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskLoss {
    /// When the recovery observed the host's absence.
    pub observed_at_ms: u64,
    /// The status the lost host last recorded, preserved for audit.
    pub previous: BackgroundTaskStatus,
    /// What was observable about the workload at that moment.
    pub process: ProcessPresence,
}

/// A goal task whose admitting runtime generation is proven gone.
///
/// Unlike [`TaskLoss`], this says nothing about the process: the host observed
/// no host loss and still owns the workload. It is the ownership fact a
/// replacement generation needs before it may stop treating the task as live
/// work — and the moment its duration stops advancing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerLoss {
    /// When the replacement generation proved the admitting boot gone.
    pub observed_at_ms: u64,
    /// The boot that admitted the task, exactly as recorded at spawn.
    pub owner_boot: String,
}

/// What one recovery pass converged for a repository's durable host state.
#[derive(Clone, Debug)]
pub struct HostRecovery {
    pub observed_at_ms: u64,
    /// Archived live tasks converged to `Unknown`.
    pub tasks: Vec<RecoveredTask>,
    /// Spawn intents that never proved whether a process was launched.
    pub unresolved_intents: Vec<String>,
}

impl HostRecovery {
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty() && self.unresolved_intents.is_empty()
    }
}

/// One task the recovery converged, in the terms the user can act on.
#[derive(Clone, Debug)]
pub struct RecoveredTask {
    pub task_id: String,
    pub program: String,
    pub args: Vec<String>,
    pub previous: BackgroundTaskStatus,
    pub process: ProcessPresence,
}

/// What a probe found, and what it had to reconcile to answer.
pub struct HostProbe {
    /// The live host, when one answered with the recorded instance.
    pub client: Option<ExecutionHostClient>,
    /// Residue of a host that is provably gone, converged in this probe.
    pub recovery: Option<HostRecovery>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Ready {
    address: std::net::SocketAddr,
    token: String,
    instance: String,
    pid: u32,
    repo: PathBuf,
    major: u32,
    minor: u32,
    capabilities: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Envelope {
    token: String,
    major: u32,
    request: Request,
}
#[derive(Serialize, Deserialize)]
enum Request {
    Hello,
    List,
    ShutdownIfIdle,
    Acknowledge {
        id: String,
        owner: String,
        #[serde(default)]
        unrestricted: bool,
    },
    Spawn {
        id: String,
        // Boxed: the spawn request carries argv/cwd/env, which dwarfs every
        // other frame and would make each `Request` cost its size.
        request: Box<ProcessRequest>,
        owner: String,
        writer: String,
        lifetime: BackgroundTaskLifetime,
        /// Durable attribution of the tool call and goal that admitted this
        /// process. Persisted with the started fact, so a runtime that never
        /// received the spawn reply can still attribute the task.
        #[serde(default)]
        origin: crate::BackgroundTaskOrigin,
        /// The runtime boot that admitted the task. Absent from clients that
        /// predate owner-loss recovery; only a goal task uses it.
        #[serde(default)]
        owner_boot: Option<String>,
    },
    /// Record that a goal task's admitting runtime generation is gone.
    ///
    /// The sender has proven the boot dead (the boot lease is the authority);
    /// the host only verifies that the claim names the boot this task was
    /// admitted by. It never signals, forgets or reclassifies the process.
    OwnerLost {
        id: String,
        owner: String,
        owner_boot: String,
    },
    Get {
        id: String,
        owner: String,
        #[serde(default)]
        unrestricted: bool,
    },
    Stop {
        id: String,
        owner: String,
        #[serde(default)]
        unrestricted: bool,
    },
    TerminalWrite {
        id: String,
        owner: String,
        data: Vec<u8>,
    },
    TerminalResize {
        id: String,
        owner: String,
        size: crate::command::PtySize,
    },
    TerminalObserve {
        id: String,
        owner: String,
        cursor: u64,
        max_bytes: usize,
    },
    Observe {
        id: String,
        owner: String,
        #[serde(default)]
        unrestricted: bool,
        cursor: Option<u64>,
        max_bytes: usize,
        wait_ms: u64,
    },
}
#[derive(Serialize, Deserialize)]
enum Reply {
    Acknowledged,
    Hello {
        major: u32,
        minor: u32,
        capabilities: Vec<String>,
        instance: String,
    },
    Tasks(Vec<HostedTask>),
    Task(BackgroundTaskSnapshot),
    Observation(BackgroundTaskObservation),
    TerminalObservation(crate::background::TerminalObservation),
    SpawnRejected(String),
    SpawnUnknown(String),
    Error(String),
}

/// The lifetime a peer that speaks `peer_major` is told about.
///
/// A legacy host predates the `Runtime` owner tag and only admits
/// `Session`/`Persistent`. `Persistent` is its exact name for the same contract
/// — host-owned, survives runtime replacement, ended only by an explicit stop —
/// so a Runtime service must not be refused just because the persistent owner
/// has not been upgraded yet.
fn hosted_wire_lifetime(
    peer_major: u32,
    lifetime: BackgroundTaskLifetime,
) -> BackgroundTaskLifetime {
    if peer_major < PROTOCOL_MAJOR && lifetime == BackgroundTaskLifetime::Runtime {
        BackgroundTaskLifetime::Persistent
    } else {
        lifetime
    }
}

/// Whether a request asks for control outside its owner scope.
fn request_is_unrestricted(request: &Request) -> bool {
    match request {
        Request::Acknowledge { unrestricted, .. }
        | Request::Get { unrestricted, .. }
        | Request::Stop { unrestricted, .. }
        | Request::Observe { unrestricted, .. } => *unrestricted,
        _ => false,
    }
}

fn check_compatibility(major: u32, capabilities: &[String]) -> Result<(), String> {
    if major != PROTOCOL_MAJOR && !SUPPORTED_LEGACY_MAJORS.contains(&major) {
        return Err(format!(
            "Execution Host protocol major {major} is incompatible with {PROTOCOL_MAJOR}; existing services were preserved"
        ));
    }
    for required in REQUIRED_CAPABILITIES {
        if !capabilities.iter().any(|c| c == required) {
            return Err(format!(
                "Execution Host missing capability {required}; existing services were preserved"
            ));
        }
    }
    Ok(())
}
pub(crate) fn fresh_id() -> Result<String, String> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| e.to_string())?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}
fn secure_dir(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        if !path.exists() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(path).map_err(|e| e.to_string())?;
        }
        let m = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !m.is_dir() || m.uid() != nix::unistd::geteuid().as_raw() || m.mode() & 0o077 != 0 {
            return Err(
                "Execution Host state directory must be owner-only and not a symlink".into(),
            );
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err("persistent Execution Host requires an audited owner-only state directory backend on this platform".into())
    }
}
fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    use std::io::Write;
    let parent = path.parent().ok_or("missing state parent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    serde_json::to_writer(&mut file, value).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.to_string())?;
    std::fs::File::open(parent)
        .and_then(|d| d.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn lock(path: &Path) -> Result<std::fs::File, String> {
    try_lock(path)?.ok_or_else(|| "Execution Host lifecycle lock is busy".to_string())
}

/// Take the lock when it is free, so a caller can tell "held by a live owner"
/// from "I may act". A lock error that is not contention is also reported as
/// held: unknown ownership is never guessed at either.
fn try_lock(path: &Path) -> Result<Option<std::fs::File>, String> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let f = options.open(path).map_err(|e| e.to_string())?;
    match f.try_lock_exclusive() {
        Ok(()) => Ok(Some(f)),
        Err(_) => Ok(None),
    }
}
fn ready(config: &ExecutionHostConfig) -> Result<Ready, String> {
    secure_dir(&config.state_dir)?;
    let path = config.state_dir.join("ready.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("Execution Host unavailable: {e}"))?;
        if !m.is_file() || m.uid() != nix::unistd::geteuid().as_raw() || m.mode() & 0o077 != 0 {
            return Err("insecure Execution Host ready file".into());
        }
    }
    let r: Ready = serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if r.repo != config.repo_root || !r.address.ip().is_loopback() {
        return Err("Execution Host repository/endpoint mismatch".into());
    }
    check_compatibility(r.major, &r.capabilities)?;
    Ok(r)
}
async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let n = stream.read_u32().await.map_err(|e| e.to_string())? as usize;
    if n > FRAME_LIMIT {
        return Err("Execution Host frame exceeds limit".into());
    }
    let mut b = vec![0; n];
    stream.read_exact(&mut b).await.map_err(|e| e.to_string())?;
    Ok(b)
}
async fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<(), String> {
    let b = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if b.len() > FRAME_LIMIT {
        return Err("Execution Host frame exceeds limit".into());
    }
    stream
        .write_u32(b.len() as u32)
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(&b).await.map_err(|e| e.to_string())
}

/// Distinguish an OS-proven refusal from a request whose side effect is unknown.
#[derive(Debug)]
pub enum SpawnError {
    Rejected(String),
    OutcomeUnknown(String),
}
impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(e) | Self::OutcomeUnknown(e) => f.write_str(e),
        }
    }
}
impl std::error::Error for SpawnError {}

/// What the host's own durable records say about the processes it owned.
///
/// `tasks.json` archives every admitted task; `intents.json` holds the spawn
/// intents persisted immediately before an OS spawn. They are the only
/// authority on whether an unreachable host may still own something, so every
/// caller that must decide between "residue" and "unknown" reads them here.
pub struct HostRecords {
    tasks: Vec<HostedTask>,
    active_intents: Vec<SpawnIntent>,
    unresolved_intents: Vec<UnresolvedIntent>,
}

impl HostRecords {
    /// `Err` means the records cannot be read as truth: an unreadable or
    /// unparsable archive proves nothing and is never treated as empty.
    pub fn read(state_dir: &Path) -> Result<Self, String> {
        let tasks = match std::fs::read(state_dir.join("tasks.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid Execution Host task archive: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("could not read Execution Host task archive: {e}")),
        };
        let active_intents = match std::fs::read(state_dir.join("intents.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid Execution Host spawn intents: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("could not read Execution Host spawn intents: {e}")),
        };
        let unresolved_intents = match std::fs::read(state_dir.join("intents.unresolved.json")) {
            Ok(bytes) => serde_json::from_slice::<Vec<UnresolvedIntent>>(&bytes)
                .map_err(|e| format!("invalid unresolved Execution Host spawn intents: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(format!(
                    "could not read unresolved Execution Host spawn intents: {e}"
                ));
            }
        };
        Ok(Self {
            tasks,
            active_intents,
            unresolved_intents,
        })
    }

    /// A task the archive still holds as live is ownership, never residue.
    pub fn has_unmanaged_process(&self) -> bool {
        self.tasks.iter().any(HostedTask::is_live)
    }

    /// A spawn whose OS result was never recorded may still be running.
    /// Recovery archives these before a runtime may start.
    pub fn intents_outstanding(&self) -> bool {
        !self.active_intents.is_empty()
    }

    /// This task's spawn may have been launched but never recorded. It is never
    /// treated as "never admitted".
    pub fn has_active_intent(&self, task_id: &str) -> bool {
        self.active_intents
            .iter()
            .any(|intent| intent.id == task_id)
    }

    /// Whether the host ever admitted this task as a process fact.
    pub fn has_task(&self, task_id: &str) -> bool {
        self.task(task_id).is_some()
    }

    pub fn task(&self, task_id: &str) -> Option<&HostedTask> {
        self.tasks.iter().find(|task| task.snapshot.id == task_id)
    }

    /// The archived process facts, including terminal ones whose controlling
    /// host is gone. Reading them is not ownership: callers must not signal or
    /// adopt from this list.
    pub fn tasks(&self) -> &[HostedTask] {
        &self.tasks
    }

    /// A recorded spawn intent whose launch was never proven either way. It owns
    /// no process fact, but it may own a process, so it is never treated as
    /// "never admitted".
    pub fn has_unresolved_intent(&self, task_id: &str) -> bool {
        self.unresolved_intents
            .iter()
            .any(|unresolved| unresolved.intent.id == task_id)
    }
}

/// Why a client could not reach the endpoint a ready file records.
#[derive(Debug)]
enum ConnectFailure {
    /// A host answered, or its own metadata spoke: never residue of a dead one.
    Metadata(String),
    /// Nothing accepted the connection the ready file points at.
    Refused(String),
}

impl ConnectFailure {
    fn message(self) -> String {
        match self {
            Self::Metadata(error) | Self::Refused(error) => error,
        }
    }
}

/// What the OS can still say about a lost host's process group.
///
/// The recorded pid is the group leader (every spawn uses `process_group(0)`,
/// terminal tasks additionally own a session), so absence of the group and its
/// session is the only proof available without the host that owned them. Every
/// other answer — present, `EPERM`, a probe this platform cannot run — is not
/// proof, and recovery is not allowed to guess.
fn process_presence(pid: Option<u32>) -> ProcessPresence {
    #[cfg(unix)]
    {
        use nix::{errno::Errno, sys::signal::killpg, unistd::Pid};
        let Some(pid) = pid else {
            return ProcessPresence::Unobservable;
        };
        match killpg(Pid::from_raw(pid as i32), None) {
            Err(Errno::ESRCH) => {}
            Ok(()) | Err(_) => return ProcessPresence::Present,
        }
        // A terminal task owns a session: job-control shells create further
        // process groups inside it, so an empty group alone is not the end.
        match crate::pty::session_members(pid as i32) {
            Ok(members) if members.is_empty() => ProcessPresence::Gone,
            Ok(_) => ProcessPresence::Present,
            Err(_) => ProcessPresence::Unobservable,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        ProcessPresence::Unobservable
    }
}

/// Evidence about a *recovered* task's process, using its durable start token.
///
/// A live group probe alone cannot tell the recorded workload from a recycled
/// pid. The token is therefore checked first. A mismatch proves the recorded
/// process is gone; it still never authorizes a signal to whatever holds the id
/// now, and any surviving group member keeps the answer conservative.
fn task_process_presence(task: &HostedTask) -> ProcessPresence {
    let Some(pid) = task.snapshot.pid else {
        return ProcessPresence::Unobservable;
    };
    let Some(expected) = task.snapshot.start_identity.as_deref() else {
        // No start token: a pid is not identity. Presence may still be probed
        // for a conservative "may exist" answer, never for adoption or kill.
        return process_presence(Some(pid));
    };
    match crate::process_identity::verify_process_identity(pid, expected) {
        crate::process_identity::ProcessIdentity::Matches => process_presence(Some(pid)),
        // The recorded leader is gone. A group or session member may still
        // exist; only its absence proves the whole workload is over.
        crate::process_identity::ProcessIdentity::Exited
        | crate::process_identity::ProcessIdentity::Mismatch { .. } => process_presence(Some(pid)),
        crate::process_identity::ProcessIdentity::Unknown => ProcessPresence::Unobservable,
    }
}

/// Reconcile the durable records of a host that is provably gone.
///
/// The lifecycle lock is the authority: `serve` holds it for the host's whole
/// life and the kernel drops it when the holder dies, so a lock this process can
/// take proves the owner is gone. `Ok(None)` therefore means "not mine to
/// recover" — either a live host still holds the lock, or there is no residue.
///
/// Live archive records are converged to `Unknown` with the observed evidence,
/// never to a fabricated exit: the host can no longer observe an exit code, so
/// the only honest terminal fact is that control is lost. A workload that may
/// still exist is recorded as present and is never signalled, adopted or
/// replayed. Spawn intents that no host can resolve move to
/// `intents.unresolved.json`, keeping their evidence out of the way of startup
/// without pretending a process fact exists.
fn recover_dead_host(config: &ExecutionHostConfig) -> Result<Option<HostRecovery>, String> {
    let dir = &config.state_dir;
    if !dir.exists() {
        return Ok(None);
    }
    // An empty namespace has nothing to recover; do not touch it (or validate
    // it) merely because a command asked about liveness.
    let has_residue = dir.join("ready.json").exists()
        || dir.join("tasks.json").exists()
        || dir.join("intents.json").exists();
    if !has_residue {
        return Ok(None);
    }
    // Recovery writes durable facts: the directory must still be this user's,
    // owner-only and not a symlink. An insecure state dir is reported, not
    // rewritten.
    secure_dir(dir)?;
    let Some(_owner) = try_lock(&dir.join("host.lock"))? else {
        return Ok(None);
    };
    let records = HostRecords::read(&config.state_dir)?;
    let ready_path = config.state_dir.join("ready.json");
    let intents = active_intents(&config.state_dir)?;
    if !records.has_unmanaged_process() && intents.is_empty() && !ready_path.exists() {
        return Ok(None);
    }
    let observed_at_ms = wall_ms();
    let mut tasks = records.tasks;
    let mut recovery = HostRecovery {
        observed_at_ms,
        tasks: Vec::new(),
        unresolved_intents: Vec::new(),
    };
    for task in tasks.iter_mut().filter(|task| task.is_live()) {
        let previous = task.snapshot.status;
        let process = task_process_presence(task);
        recovery.tasks.push(RecoveredTask {
            task_id: task.snapshot.id.clone(),
            program: task.snapshot.program.clone(),
            args: task.snapshot.args.clone(),
            previous,
            process,
        });
        task.snapshot.status = BackgroundTaskStatus::Unknown;
        task.snapshot.duration_ms = observed_at_ms.saturating_sub(task.started_at_ms);
        task.finished_at_ms = Some(observed_at_ms);
        // No completion was committed, so no exit fact may survive the
        // convergence. A partially observed code or signal in a Running record
        // is not a terminal truth: the CompletionRecord is the commit point,
        // and presenting a code beside `Unknown` would invent one.
        task.snapshot.exit_code = None;
        task.snapshot.signal = None;
        task.result = None;
        task.loss = Some(TaskLoss {
            observed_at_ms,
            previous,
            process,
        });
    }
    let converged = !recovery.tasks.is_empty();
    if converged {
        atomic_json(&config.state_dir.join("tasks.json"), &tasks)?;
    }
    if !intents.is_empty() {
        let mut unresolved = match std::fs::read(config.state_dir.join("intents.unresolved.json")) {
            Ok(bytes) => serde_json::from_slice::<Vec<UnresolvedIntent>>(&bytes)
                .map_err(|e| format!("invalid unresolved Execution Host spawn intents: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("could not read unresolved spawn intents: {e}")),
        };
        for intent in intents {
            recovery.unresolved_intents.push(intent.id.clone());
            if unresolved
                .iter()
                .any(|existing| existing.intent.id == intent.id)
            {
                continue;
            }
            unresolved.push(UnresolvedIntent {
                intent,
                observed_at_ms,
            });
        }
        // Archive before clearing: a crash between the two leaves the intent
        // both active and archived, and the next pass dedupes it by id.
        atomic_json(
            &config.state_dir.join("intents.unresolved.json"),
            &unresolved,
        )?;
        atomic_json(
            &config.state_dir.join("intents.json"),
            &Vec::<SpawnIntent>::new(),
        )?;
    }
    match std::fs::remove_file(&ready_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!("could not remove {}: {e}", ready_path.display()));
        }
    }
    for task in &recovery.tasks {
        tracing::warn!(
            task = %task.task_id,
            program = %task.program,
            previous = ?task.previous,
            process = ?task.process,
            "Execution Host was lost; task converged to unobserved state"
        );
    }
    for task_id in &recovery.unresolved_intents {
        tracing::warn!(
            task = %task_id,
            "Execution Host spawn intent kept as unresolved; its launch was never proven"
        );
    }
    Ok(Some(recovery))
}

fn active_intents(state_dir: &Path) -> Result<Vec<SpawnIntent>, String> {
    match std::fs::read(state_dir.join("intents.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("invalid Execution Host spawn intents: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("could not read Execution Host spawn intents: {e}")),
    }
}

/// Bytes of a failed host's stderr retained for the readiness diagnostic.
const STDERR_DIAGNOSTIC_LIMIT: usize = 8 * 1024;

/// Drain a child host's stderr on its own thread.
///
/// The host reports through tracing on stderr; an undrained pipe would block
/// it once the pipe buffer filled. Every byte is consumed, while the retained
/// diagnostic keeps only a bounded tail.
fn capture_child_stderr(
    child: &mut std::process::Child,
) -> (
    Arc<std::sync::Mutex<Vec<u8>>>,
    std::sync::mpsc::Receiver<()>,
) {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (done, finished) = std::sync::mpsc::channel();
    let Some(mut pipe) = child.stderr.take() else {
        return (captured, finished);
    };
    let sink = Arc::clone(&captured);
    std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let mut sink = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    sink.extend_from_slice(&buffer[..count]);
                    let excess = sink.len().saturating_sub(STDERR_DIAGNOSTIC_LIMIT);
                    if excess > 0 {
                        sink.drain(..excess);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = done.send(());
    });
    (captured, finished)
}

/// The readiness error plus the host's own last words.
///
/// The wait is diagnostic, not lifecycle: a descendant that inherited the
/// stderr pipe must not hold the start failure open.
fn readiness_failure(
    reason: String,
    captured: &Arc<std::sync::Mutex<Vec<u8>>>,
    finished: &std::sync::mpsc::Receiver<()>,
) -> String {
    let _ = finished.recv_timeout(Duration::from_secs(2));
    let captured = captured
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let stderr = String::from_utf8_lossy(&captured);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        reason
    } else {
        format!("{reason}; stderr: {stderr}")
    }
}

#[derive(Clone)]
pub struct ExecutionHostClient {
    config: ExecutionHostConfig,
    ready: Ready,
    /// The major the connected host declared. A legacy host has no
    /// `unrestricted` control plane, so that path stays refused rather than
    /// silently degrading to an owner-scoped permission check.
    peer_major: u32,
    /// Optional capabilities the host declared in its handshake. Required
    /// capabilities are checked at connect; these gate additive requests such
    /// as owner-loss recovery.
    peer_capabilities: Vec<String>,
}
impl ExecutionHostClient {
    /// Read a live host, or recover the residue of one that is gone.
    ///
    /// A ready file is a derived fact, not an owner: its existence says nothing
    /// by itself. When the recorded endpoint refuses the connection, the
    /// lifecycle lock is the authority. A lock this process can take proves the
    /// owner is gone, so its records are reconciled from OS evidence and the
    /// endpoint is removed; a held lock means an owner may still exist and its
    /// state stays unknown. A ready file answered by its own metadata (for
    /// example an incompatible major) is never residue.
    pub async fn probe(config: &ExecutionHostConfig) -> Result<HostProbe, String> {
        // A refused endpoint may coincide with another reader's recovery pass.
        // The lifecycle lock then makes our own attempt return "not mine to
        // recover", which is not a read failure: wait for that owner to finish
        // and re-read. This keeps the first observation after a host loss from
        // surfacing a transient "unavailable" error to the user.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let ready_path = config.state_dir.join("ready.json");
            if ready_path.exists() {
                match Self::connect_classified(config.clone()).await {
                    Ok(client) => {
                        return Ok(HostProbe {
                            client: Some(client),
                            recovery: None,
                        });
                    }
                    Err(ConnectFailure::Metadata(error)) => return Err(error),
                    Err(ConnectFailure::Refused(error)) => {
                        match recover_dead_host(config)? {
                            Some(recovery) => {
                                return Ok(HostProbe {
                                    client: None,
                                    recovery: Some(recovery),
                                });
                            }
                            None if !ready_path.exists() => {
                                // Another pass converged the residue while we
                                // waited for the lock; the archive is now the
                                // durable inventory.
                                return Ok(HostProbe {
                                    client: None,
                                    recovery: None,
                                });
                            }
                            None => {
                                if tokio::time::Instant::now() >= deadline {
                                    return Err(error);
                                }
                                tokio::time::sleep(Duration::from_millis(20)).await;
                                continue;
                            }
                        }
                    }
                }
            }
            // No endpoint, but a crash before readiness (or a spawn that never
            // recorded its fact) can still leave records no host will resolve.
            let recovery = recover_dead_host(config)?;
            return Ok(HostProbe {
                client: None,
                recovery,
            });
        }
    }
    pub async fn connect(config: ExecutionHostConfig) -> Result<Self, String> {
        Self::connect_classified(config)
            .await
            .map_err(ConnectFailure::message)
    }

    async fn connect_classified(config: ExecutionHostConfig) -> Result<Self, ConnectFailure> {
        let r = ready(&config).map_err(ConnectFailure::Metadata)?;
        let mut c = Self {
            peer_major: r.major,
            peer_capabilities: Vec::new(),
            config,
            ready: r,
        };
        match c.call_classified(Request::Hello).await? {
            Reply::Hello {
                major,
                capabilities,
                instance,
                ..
            } => {
                check_compatibility(major, &capabilities).map_err(ConnectFailure::Metadata)?;
                if instance != c.ready.instance {
                    return Err(ConnectFailure::Metadata(
                        "Execution Host instance changed".into(),
                    ));
                }
                if major != c.peer_major {
                    return Err(ConnectFailure::Metadata(
                        "Execution Host major changed since readiness".into(),
                    ));
                }
                c.peer_capabilities = capabilities;
                Ok(c)
            }
            _ => Err(ConnectFailure::Metadata(
                "invalid Execution Host handshake".into(),
            )),
        }
    }

    /// The major the connected host speaks.
    pub fn peer_major(&self) -> u32 {
        self.peer_major
    }

    /// Whether the connected host declared an optional capability.
    pub fn supports(&self, capability: &str) -> bool {
        self.peer_capabilities
            .iter()
            .any(|declared| declared == capability)
    }

    fn require_unrestricted_control(&self) -> Result<(), String> {
        if self.peer_major < PROTOCOL_MAJOR {
            return Err(format!(
                "Execution Host protocol major {} has no unrestricted control; \
                 the task must be managed by its owner",
                self.peer_major
            ));
        }
        Ok(())
    }
    pub async fn ensure(config: ExecutionHostConfig) -> Result<Self, String> {
        secure_dir(&config.state_dir)?;
        if config.state_dir.join("ready.json").exists() {
            match Self::connect(config.clone()).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    // Never replace a live host on incompatibility or a transient connection failure.
                    // Only residue whose owner is provably gone is recovered, and
                    // that recovery preserves every record and never signals.
                    match recover_dead_host(&config) {
                        Ok(Some(recovery)) => {
                            tracing::warn!(
                                tasks = recovery.tasks.len(),
                                intents = recovery.unresolved_intents.len(),
                                "recovered a lost Execution Host before starting a new one"
                            );
                        }
                        Ok(None) => return Err(e),
                        Err(reason) => {
                            return Err(format!("{e}; recovery refused: {reason}"));
                        }
                    }
                }
            }
        } else {
            // No endpoint: reconcile residue that no live host can resolve.
            if let Some(recovery) = recover_dead_host(&config)? {
                tracing::warn!(
                    tasks = recovery.tasks.len(),
                    intents = recovery.unresolved_intents.len(),
                    "recovered ownerless Execution Host residue before starting a host"
                );
            }
        }
        let _startup = lock(&config.state_dir.join("startup.lock"))?;
        if let Ok(c) = Self::connect(config.clone()).await {
            return Ok(c);
        }
        let mut command = std::process::Command::new(&config.executable);
        command
            .arg("execution-host")
            .arg("--state-dir")
            .arg(&config.state_dir)
            .arg("--repo")
            .arg(&config.repo_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x00000008 | 0x00000200);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("could not launch Execution Host: {e}"))?;
        let (stderr_tail, stderr_done) = capture_child_stderr(&mut child);
        // A cold host on a loaded runner (a CI shard starting several real
        // runtimes) can need more than ten seconds to bind and answer. The
        // wait only delays the visible fallback when the host is genuinely
        // broken; a host that exits early is detected immediately by
        // `try_wait`.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(c) = Self::connect(config.clone()).await {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(c);
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Err(readiness_failure(
                    format!("Execution Host exited before readiness: {status}"),
                    &stderr_tail,
                    &stderr_done,
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(readiness_failure(
                    "Execution Host readiness timed out".into(),
                    &stderr_tail,
                    &stderr_done,
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    async fn call(&self, request: Request) -> Result<Reply, String> {
        self.call_classified(request)
            .await
            .map_err(ConnectFailure::message)
    }
    async fn call_classified(&self, request: Request) -> Result<Reply, ConnectFailure> {
        tokio::time::timeout(Duration::from_secs(35), async {
            let mut s = TcpStream::connect(self.ready.address).await.map_err(|e| {
                ConnectFailure::Refused(format!(
                    "Execution Host unavailable; service state is unknown: {e} \
                     (recorded endpoint {}, pid {}, instance {})",
                    self.ready.address, self.ready.pid, self.ready.instance
                ))
            })?;
            write_frame(
                &mut s,
                &Envelope {
                    token: self.ready.token.clone(),
                    major: self.peer_major,
                    request,
                },
            )
            .await
            .map_err(ConnectFailure::Metadata)?;
            let reply: Reply = serde_json::from_slice(
                &read_frame(&mut s).await.map_err(ConnectFailure::Metadata)?,
            )
            .map_err(|e| ConnectFailure::Metadata(e.to_string()))?;
            match reply {
                Reply::Error(e) => Err(ConnectFailure::Metadata(e)),
                v => Ok(v),
            }
        })
        .await
        .map_err(|_| {
            ConnectFailure::Metadata("Execution Host request timeout; outcome unknown".to_string())
        })?
    }
    pub async fn list(&self) -> Result<Vec<HostedTask>, String> {
        match self.call(Request::List).await? {
            Reply::Tasks(t) => Ok(t),
            _ => Err("invalid Execution Host list reply".into()),
        }
    }
    pub fn require_terminal(&self) -> Result<(), String> {
        for capability in [
            "terminal.pty",
            "terminal.input",
            "terminal.resize",
            "terminal.raw-observe",
        ] {
            if !self.ready.capabilities.iter().any(|c| c == capability) {
                return Err(format!(
                    "Unsupported: Execution Host missing {capability}; existing services preserved"
                ));
            }
        }
        Ok(())
    }
    pub async fn terminal_write(&self, id: &str, owner: &str, data: Vec<u8>) -> Result<(), String> {
        self.require_terminal()?;
        match self
            .call(Request::TerminalWrite {
                id: id.into(),
                owner: owner.into(),
                data,
            })
            .await?
        {
            Reply::Acknowledged => Ok(()),
            _ => Err("invalid terminal write reply".into()),
        }
    }
    pub async fn terminal_resize(
        &self,
        id: &str,
        owner: &str,
        size: crate::command::PtySize,
    ) -> Result<(), String> {
        self.require_terminal()?;
        match self
            .call(Request::TerminalResize {
                id: id.into(),
                owner: owner.into(),
                size,
            })
            .await?
        {
            Reply::Acknowledged => Ok(()),
            _ => Err("invalid terminal resize reply".into()),
        }
    }
    pub async fn terminal_observe(
        &self,
        id: &str,
        owner: &str,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<crate::background::TerminalObservation, String> {
        self.require_terminal()?;
        match self
            .call(Request::TerminalObserve {
                id: id.into(),
                owner: owner.into(),
                cursor,
                max_bytes,
            })
            .await?
        {
            Reply::TerminalObservation(value) => Ok(value),
            _ => Err("invalid terminal observation reply".into()),
        }
    }
    pub async fn spawn(
        &self,
        id: &str,
        request: ProcessRequest,
        owner: &str,
        writer: &str,
        lifetime: BackgroundTaskLifetime,
        origin: crate::BackgroundTaskOrigin,
        owner_boot: Option<&str>,
    ) -> Result<BackgroundTaskSnapshot, SpawnError> {
        // A legacy host predates the `Runtime` owner tag and only admits
        // `Session`/`Persistent`. `Persistent` is its exact name for the same
        // contract — host-owned, survives runtime replacement, ended only by an
        // explicit stop — so a Runtime service must not be refused (or silently
        // fall back to a runtime-local process) just because the persistent
        // owner has not been upgraded yet.
        if request.pty.is_some() {
            self.require_terminal().map_err(SpawnError::Rejected)?;
        }
        let lifetime = hosted_wire_lifetime(self.peer_major, lifetime);
        match self
            .call(Request::Spawn {
                id: id.into(),
                request: Box::new(request),
                owner: owner.into(),
                writer: writer.into(),
                lifetime,
                origin,
                owner_boot: owner_boot.map(str::to_string),
            })
            .await
            .map_err(SpawnError::OutcomeUnknown)?
        {
            Reply::Task(t) => Ok(t),
            Reply::SpawnRejected(e) => Err(SpawnError::Rejected(e)),
            Reply::SpawnUnknown(e) => Err(SpawnError::OutcomeUnknown(e)),
            _ => Err(SpawnError::OutcomeUnknown(
                "invalid Execution Host spawn reply".into(),
            )),
        }
    }

    /// Record that a goal task's admitting runtime generation is gone.
    ///
    /// The boot liveness proof belongs to the caller. This request only names
    /// the fact; the host verifies the task identity and persists it without
    /// touching the process. Idempotent: an already-marked task is
    /// acknowledged and its original observation time is kept.
    pub async fn owner_lost(&self, id: &str, owner: &str, owner_boot: &str) -> Result<(), String> {
        if !self.supports(OWNER_LOSS_CAPABILITY) {
            return Err(format!(
                "Execution Host does not declare {OWNER_LOSS_CAPABILITY}; owner loss cannot be recorded"
            ));
        }
        match self
            .call(Request::OwnerLost {
                id: id.into(),
                owner: owner.into(),
                owner_boot: owner_boot.into(),
            })
            .await?
        {
            Reply::Acknowledged => Ok(()),
            _ => Err("invalid Execution Host owner-loss reply".into()),
        }
    }

    /// Safe host replacement boundary; never terminates an active workload.
    pub async fn shutdown_if_idle(&self) -> Result<(), String> {
        match self.call(Request::ShutdownIfIdle).await? {
            Reply::Acknowledged => Ok(()),
            _ => Err("invalid host shutdown acknowledgement".into()),
        }
    }
    pub async fn acknowledge(&self, id: &str, owner: &str) -> Result<(), String> {
        match self
            .call(Request::Acknowledge {
                id: id.into(),
                owner: owner.into(),
                unrestricted: false,
            })
            .await?
        {
            Reply::Acknowledged => Ok(()),
            _ => Err("invalid process fact acknowledgement".into()),
        }
    }
    pub async fn acknowledge_unrestricted(&self, id: &str) -> Result<(), String> {
        self.require_unrestricted_control()?;
        match self
            .call(Request::Acknowledge {
                id: id.into(),
                owner: String::new(),
                unrestricted: true,
            })
            .await?
        {
            Reply::Acknowledged => Ok(()),
            _ => Err("invalid process fact acknowledgement".into()),
        }
    }
    pub async fn get_owned(&self, id: &str, owner: &str) -> Result<BackgroundTaskSnapshot, String> {
        match self
            .call(Request::Get {
                id: id.into(),
                owner: owner.into(),
                unrestricted: false,
            })
            .await?
        {
            Reply::Task(t) => Ok(t),
            _ => Err("invalid Execution Host inspect reply".into()),
        }
    }
    pub async fn get_unrestricted(&self, id: &str) -> Result<BackgroundTaskSnapshot, String> {
        self.require_unrestricted_control()?;
        match self
            .call(Request::Get {
                id: id.into(),
                owner: String::new(),
                unrestricted: true,
            })
            .await?
        {
            Reply::Task(t) => Ok(t),
            _ => Err("invalid Execution Host inspect reply".into()),
        }
    }
    pub async fn kill_owned(
        &self,
        id: &str,
        owner: &str,
    ) -> Result<BackgroundTaskSnapshot, String> {
        match self
            .call(Request::Stop {
                id: id.into(),
                owner: owner.into(),
                unrestricted: false,
            })
            .await?
        {
            Reply::Task(t) => Ok(t),
            _ => Err("invalid Execution Host stop reply".into()),
        }
    }
    pub async fn kill_unrestricted(&self, id: &str) -> Result<BackgroundTaskSnapshot, String> {
        self.require_unrestricted_control()?;
        match self
            .call(Request::Stop {
                id: id.into(),
                owner: String::new(),
                unrestricted: true,
            })
            .await?
        {
            Reply::Task(t) => Ok(t),
            _ => Err("invalid Execution Host stop reply".into()),
        }
    }
    pub async fn observe_owned(
        &self,
        id: &str,
        owner: &str,
        cursor: Option<u64>,
        max_bytes: usize,
        wait: Duration,
    ) -> Result<BackgroundTaskObservation, String> {
        match self
            .call(Request::Observe {
                id: id.into(),
                owner: owner.into(),
                unrestricted: false,
                cursor,
                max_bytes,
                wait_ms: wait.as_millis().min(30_000) as u64,
            })
            .await?
        {
            Reply::Observation(t) => Ok(t),
            _ => Err("invalid Execution Host observe reply".into()),
        }
    }
    pub async fn observe_unrestricted(
        &self,
        id: &str,

        cursor: Option<u64>,
        max_bytes: usize,
        wait: Duration,
    ) -> Result<BackgroundTaskObservation, String> {
        self.require_unrestricted_control()?;
        match self
            .call(Request::Observe {
                id: id.into(),
                owner: String::new(),
                unrestricted: true,
                cursor,
                max_bytes,
                wait_ms: wait.as_millis().min(30_000) as u64,
            })
            .await?
        {
            Reply::Observation(t) => Ok(t),
            _ => Err("invalid Execution Host observe reply".into()),
        }
    }
    pub fn state_dir(&self) -> &Path {
        &self.config.state_dir
    }
}
struct Entry {
    local_id: String,
    owner: String,
    writer: String,
    scope: WriteScope,
    fingerprint: String,
    lifetime: BackgroundTaskLifetime,
    started_at_ms: u64,
    owner_boot: Option<String>,
    owner_loss: Option<OwnerLoss>,
}
#[derive(Clone, Serialize, Deserialize)]
struct SpawnIntent {
    id: String,
    owner: String,
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
    started_at_ms: u64,
}

/// A spawn intent that outlived the host which wrote it.
///
/// The intent is durable before an OS spawn and removed only after the process
/// fact is persisted. A host that died inside that window leaves a command that
/// may or may not have launched; no live host can resolve it, and discarding it
/// would delete the only evidence. It is moved here, where it blocks nothing but
/// is still inspectable.
#[derive(Clone, Serialize, Deserialize)]
struct UnresolvedIntent {
    intent: SpawnIntent,
    /// When recovery found the intent without a host to resolve it.
    observed_at_ms: u64,
}
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Debug-build crash injection for the two non-atomic spawn windows and the
/// exit window before the completion record is durable.
///
/// A test cannot time a host SIGKILL against an internal write reliably. In a
/// debug build only, the host can be told to die at one exact point: the point
/// name and a marker path must both be present, and the marker makes the
/// injection one-shot so the replacement host in the same test survives. A
/// release build does not contain these points at all.
#[cfg(debug_assertions)]
fn debug_fault(point: &str) {
    if std::env::var("LEVELER_EXECUTION_HOST_FAULT").as_deref() != Ok(point) {
        return;
    }
    let Some(marker) = std::env::var_os("LEVELER_EXECUTION_HOST_FAULT_MARKER") else {
        return;
    };
    let marker = std::path::PathBuf::from(marker);
    if marker.exists() || std::fs::write(&marker, point).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        // A real, unwinding-free death: cleanup and the persistence loop must
        // not get a chance to turn the injected window into a durable fact.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(std::process::id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(not(unix))]
    std::process::abort();
}
struct Host {
    registry: BackgroundTaskRegistry,
    entries: Mutex<HashMap<String, Entry>>,
    archive: Mutex<HashMap<String, HostedTask>>,
    acknowledged: Mutex<HashSet<String>>,
    intents: Mutex<Vec<SpawnIntent>>,
    shutdown: tokio_util::sync::CancellationToken,
    retiring: std::sync::atomic::AtomicBool,
    persistence: Mutex<()>,
    ready: Ready,
    config: ExecutionHostConfig,
}
impl Host {
    async fn tasks(&self) -> Result<Vec<HostedTask>, String> {
        let entries = self.entries.lock().await;
        let mut records = self.archive.lock().await;
        for (id, e) in entries.iter() {
            if let Some((mut snapshot, observed_pid, log_end, log_prefix_len)) =
                self.registry.process_fact(&e.local_id).await
            {
                snapshot.id = id.clone();
                let terminal = matches!(
                    snapshot.status,
                    BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
                );
                let process_id =
                    observed_pid.or_else(|| records.get(id).and_then(|t| t.process_id));
                let finished_at_ms =
                    terminal.then_some(e.started_at_ms.saturating_add(snapshot.duration_ms));
                // The completion record is derived from the same read as the
                // terminal snapshot, so it cannot describe a different exit.
                let result = match finished_at_ms {
                    Some(completed_at_ms) if terminal => {
                        Some(CompletionRecord::of(id, &snapshot, completed_at_ms))
                    }
                    _ => None,
                };
                records.insert(
                    id.clone(),
                    HostedTask {
                        snapshot,
                        lifetime: e.lifetime,
                        writer_scope: e.writer.clone(),
                        write_scope: e.scope.clone(),
                        process_id,
                        started_at_ms: e.started_at_ms,
                        finished_at_ms,
                        log_end,
                        log_prefix_len,
                        owner_boot: e.owner_boot.clone(),
                        owner_loss: e.owner_loss.clone(),
                        // This host observed the entry; the exit fact is its own.
                        loss: None,
                        result,
                    },
                );
            }
        }
        // The local registry uses Runtime to avoid recursive hosting. The host
        // owns the public contract, including records archived by older hosts.
        for task in records.values_mut() {
            task.snapshot.lifetime = task.lifetime;
        }
        let mut out = records.values().cloned().collect::<Vec<_>>();
        out.sort_by(|a, b| a.started_at_ms.cmp(&b.started_at_ms));
        Ok(out)
    }
    async fn persisted_tasks(&self) -> Result<Vec<HostedTask>, String> {
        let _serialization = self.persistence.lock().await;
        let tasks = self.tasks().await?;
        #[cfg(debug_assertions)]
        if tasks.iter().any(|task| {
            matches!(
                task.snapshot.status,
                BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
            )
        }) {
            debug_fault("before-terminal-persist");
        }
        atomic_json(&self.config.state_dir.join("tasks.json"), &tasks)?;
        Ok(tasks)
    }
    async fn persist(&self) -> Result<(), String> {
        self.persisted_tasks().await.map(|_| ())
    }

    async fn local(&self, id: &str, owner: &str, unrestricted: bool) -> Result<String, String> {
        let entries = self.entries.lock().await;
        let e = entries
            .get(id)
            .ok_or_else(|| format!("unknown hosted task `{id}`"))?;
        if !unrestricted && e.owner != owner {
            return Err("hosted task belongs to another session".into());
        }
        Ok(e.local_id.clone())
    }
    async fn handle(&self, request: Request) -> Result<Reply, String> {
        match request {
            Request::Hello => Ok(Reply::Hello {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
                capabilities: self.ready.capabilities.clone(),
                instance: self.ready.instance.clone(),
            }),
            Request::List => {
                let mut tasks = self.persisted_tasks().await?;
                // Inventory is bounded metadata. Full retained logs remain available
                // through inspect/observe, rather than multiplying frame size by 64.
                for task in &mut tasks {
                    task.snapshot.log.clear();
                }
                Ok(Reply::Tasks(tasks))
            }
            Request::ShutdownIfIdle => {
                let _admission = self.entries.lock().await;
                if self.registry.alive_count().await != 0 || !self.intents.lock().await.is_empty() {
                    return Err("Execution Host has active workloads; services preserved".into());
                }
                self.retiring
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(Reply::Acknowledged)
            }
            Request::Acknowledge {
                id,
                owner,
                unrestricted,
            } => {
                let records = self.archive.lock().await;
                let task = records.get(&id).ok_or("unknown process fact")?;
                if (!unrestricted && task.snapshot.owner_scope.as_deref() != Some(&owner))
                    || matches!(
                        task.snapshot.status,
                        BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
                    )
                {
                    return Err("process fact is not terminal or belongs to another session".into());
                }
                self.acknowledged.lock().await.insert(id);
                Ok(Reply::Acknowledged)
            }
            Request::Spawn {
                id,
                request,
                owner,
                writer,
                lifetime,
                origin,
                owner_boot,
            } => {
                if !id.starts_with("host-")
                    || id.len() != 37
                    || !id[5..].bytes().all(|b| b.is_ascii_hexdigit())
                    || owner.is_empty()
                {
                    return Err("invalid hosted task identity".into());
                }
                let fingerprint = {
                    use sha2::{Digest, Sha256};
                    let mut hasher = Sha256::new();
                    hasher.update(serde_json::to_vec(&request).map_err(|e| e.to_string())?);
                    hasher.update([0]);
                    hasher.update(serde_json::to_vec(&origin).map_err(|e| e.to_string())?);
                    format!("{:x}", hasher.finalize())
                };
                let mut entries = self.entries.lock().await;
                if let Some(e) = entries.get(&id) {
                    if e.owner != owner
                        || e.writer != writer
                        || e.fingerprint != fingerprint
                        || e.lifetime != lifetime
                        || e.owner_boot != owner_boot
                    {
                        return Err("hosted spawn identity belongs to another owner".into());
                    }
                    drop(entries);
                    let t = self
                        .persisted_tasks()
                        .await?
                        .into_iter()
                        .find(|t| t.snapshot.id == id)
                        .ok_or("missing spawn fact")?;
                    return Ok(Reply::Task(t.snapshot));
                }
                if self.retiring.load(std::sync::atomic::Ordering::SeqCst) {
                    return Ok(Reply::SpawnRejected("Execution Host is retiring".into()));
                }
                if self.archive.lock().await.contains_key(&id) {
                    return Ok(Reply::SpawnRejected(
                        "task id already exists in durable process history; refusing replay".into(),
                    ));
                }
                // Only clients explicitly acknowledging a terminal fact permit its later eviction.
                // The host need not understand why a client has consumed that fact.
                let ack = self.acknowledged.lock().await.clone();
                let mut records = self.archive.lock().await;
                if records.len() >= 64 {
                    let evict = records
                        .iter()
                        .filter(|(id, t)| {
                            ack.contains(*id)
                                && matches!(
                                    t.snapshot.status,
                                    BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
                                )
                        })
                        .min_by_key(|(_, t)| t.started_at_ms)
                        .map(|(id, _)| id.clone());
                    if let Some(id) = evict {
                        records.remove(&id);
                        self.acknowledged.lock().await.remove(&id);
                        if let Some(entry) = entries.remove(&id) {
                            self.registry.forget_terminal_fact(&entry.local_id).await?;
                        }
                    } else {
                        return Ok(Reply::SpawnRejected(
                            "Execution Host unacknowledged fact retention limit reached".into(),
                        ));
                    }
                }
                drop(records);
                let scope = request.write_scope.clone();
                if !matches!(
                    lifetime,
                    BackgroundTaskLifetime::Goal
                        | BackgroundTaskLifetime::Runtime
                        | BackgroundTaskLifetime::Session
                        | BackgroundTaskLifetime::Persistent
                ) {
                    return Err("unsupported lifetime for a hosted task".into());
                }
                let started_at_ms = wall_ms();
                // Intent is durable before any OS spawn. A crash here is explicitly unknown,
                // never permission to launch another copy of this workload.
                let mut intents = self.intents.lock().await;
                intents.push(SpawnIntent {
                    id: id.clone(),
                    owner: owner.clone(),
                    program: request.program.clone(),
                    args: request.args.clone(),
                    cwd: request.cwd.clone(),
                    started_at_ms,
                });
                atomic_json(&self.config.state_dir.join("intents.json"), &*intents)?;
                #[cfg(debug_assertions)]
                debug_fault("after-intent");
                let local = match self
                    .registry
                    .spawn_with_origin(
                        *request,
                        None,
                        Some(&owner),
                        &writer,
                        // The host's own registry is never itself hosted; the
                        // public lifetime is carried by `Entry` and projected
                        // from it. Runtime also prevents recursive hosting.
                        BackgroundTaskLifetime::Runtime,
                        origin,
                    )
                    .await
                {
                    Ok(id) => id,
                    Err(error) => {
                        intents.retain(|i| i.id != id);
                        atomic_json(&self.config.state_dir.join("intents.json"), &*intents)?;
                        return Ok(Reply::SpawnRejected(error));
                    }
                };
                #[cfg(debug_assertions)]
                debug_fault("after-spawn");
                entries.insert(
                    id.clone(),
                    Entry {
                        local_id: local.clone(),
                        owner: owner.clone(),
                        writer,
                        scope,
                        fingerprint,
                        lifetime,
                        started_at_ms,
                        owner_boot,
                        owner_loss: None,
                    },
                );
                drop(entries);
                if let Err(e) = self.persist().await {
                    let _ = self.registry.kill_owned(&local, &owner).await;
                    return Err(format!(
                        "host state persistence failed; spawned task stopped: {e}"
                    ));
                }
                intents.retain(|i| i.id != id);
                atomic_json(&self.config.state_dir.join("intents.json"), &*intents)?;
                let t = self
                    .persisted_tasks()
                    .await?
                    .into_iter()
                    .find(|t| t.snapshot.id == id)
                    .ok_or("missing spawn fact")?;
                Ok(Reply::Task(t.snapshot))
            }
            Request::OwnerLost {
                id,
                owner,
                owner_boot,
            } => {
                if owner_boot.is_empty() {
                    return Err("owner loss must name the admitting boot".into());
                }
                let mut entries = self.entries.lock().await;
                let entry = entries
                    .get_mut(&id)
                    .ok_or_else(|| format!("unknown hosted task `{id}`"))?;
                if entry.owner != owner {
                    return Err("hosted task belongs to another session".into());
                }
                // Only goal work belongs to a runtime generation. Session,
                // runtime and persistent services outlive their creator by
                // contract and must keep their attribution.
                if entry.lifetime != BackgroundTaskLifetime::Goal {
                    return Err("only a goal-lifetime task can lose its runtime owner".into());
                }
                if entry.owner_boot.as_deref() != Some(owner_boot.as_str()) {
                    return Err("owner boot does not match the spawn record".into());
                }
                // The first observation is the fact. A repeated recovery — from
                // this boot or a later one — keeps it, so elapsed time and the
                // durable record do not drift.
                if entry.owner_loss.is_none() {
                    entry.owner_loss = Some(OwnerLoss {
                        observed_at_ms: wall_ms(),
                        owner_boot,
                    });
                }
                drop(entries);
                // Always rewrite the archive: a first call that failed to
                // persist must not be reported as acknowledged by a retry.
                self.persist().await?;
                Ok(Reply::Acknowledged)
            }
            Request::Get {
                id,
                owner,
                unrestricted,
            } => {
                let task = self
                    .persisted_tasks()
                    .await?
                    .into_iter()
                    .find(|t| t.snapshot.id == id)
                    .ok_or("unknown hosted task")?;
                if !unrestricted && task.snapshot.owner_scope.as_deref() != Some(&owner) {
                    return Err("hosted task belongs to another session".into());
                }
                Ok(Reply::Task(task.snapshot))
            }
            Request::Stop {
                id,
                owner,
                unrestricted,
            } => {
                let persisted = self.persisted_tasks().await?;
                if let Some(task) = persisted.iter().find(|t| {
                    t.snapshot.id == id
                        && matches!(
                            t.snapshot.status,
                            BackgroundTaskStatus::Exited
                                | BackgroundTaskStatus::Killed
                                | BackgroundTaskStatus::Unknown
                        )
                }) {
                    if !unrestricted && task.snapshot.owner_scope.as_deref() != Some(&owner) {
                        return Err("hosted task belongs to another session".into());
                    }
                    return Ok(Reply::Task(task.snapshot.clone()));
                }
                let local = self.local(&id, &owner, unrestricted).await?;
                let mut t = self
                    .registry
                    .kill_authorized(&local, &owner, unrestricted)
                    .await?;
                t.lifetime = persisted
                    .iter()
                    .find(|task| task.snapshot.id == id)
                    .ok_or("missing hosted process fact")?
                    .lifetime;
                t.id = id;
                self.persist().await?;
                Ok(Reply::Task(t))
            }
            Request::TerminalWrite { id, owner, data } => {
                let local = self.local(&id, &owner, false).await?;
                self.registry.terminal_write(&local, &owner, data).await?;
                Ok(Reply::Acknowledged)
            }
            Request::TerminalResize { id, owner, size } => {
                let local = self.local(&id, &owner, false).await?;
                self.registry.terminal_resize(&local, &owner, size).await?;
                Ok(Reply::Acknowledged)
            }
            Request::TerminalObserve {
                id,
                owner,
                cursor,
                max_bytes,
            } => {
                let local = self.local(&id, &owner, false).await?;
                let mut value = self
                    .registry
                    .terminal_observe(&local, &owner, cursor, max_bytes)
                    .await?;
                value.snapshot.id = id;
                Ok(Reply::TerminalObservation(value))
            }
            Request::Observe {
                id,
                owner,
                unrestricted,
                cursor,
                max_bytes,
                wait_ms,
            } => {
                let persisted = self.persisted_tasks().await?;
                if let Some(task) = persisted.iter().find(|t| {
                    t.snapshot.id == id
                        && matches!(
                            t.snapshot.status,
                            BackgroundTaskStatus::Exited
                                | BackgroundTaskStatus::Killed
                                | BackgroundTaskStatus::Unknown
                        )
                }) {
                    if !unrestricted && task.snapshot.owner_scope.as_deref() != Some(&owner) {
                        return Err("hosted task belongs to another session".into());
                    }
                    let mut position = cursor.unwrap_or(0);
                    if position > task.log_end {
                        return Err("log cursor beyond retained process fact".into());
                    }
                    let (log, dropped_bytes) = crate::background::take_log_delta(
                        &task.snapshot.log,
                        task.log_end,
                        task.log_prefix_len,
                        &mut position,
                        max_bytes,
                    )?;
                    let mut snapshot = task.snapshot.clone();
                    snapshot.log = log;
                    return Ok(Reply::Observation(BackgroundTaskObservation {
                        snapshot,
                        settlement: None,
                        dropped_bytes,
                        next_cursor: position,
                        log_remaining: position < task.log_end,
                    }));
                }
                let local = self.local(&id, &owner, unrestricted).await?;
                let mut t = self
                    .registry
                    .observe_authorized(
                        &local,
                        &owner,
                        unrestricted,
                        cursor,
                        max_bytes.min(256 * 1024),
                        Duration::from_millis(wait_ms.min(30_000)),
                        &tokio_util::sync::CancellationToken::new(),
                    )
                    .await?;
                t.snapshot.lifetime = persisted
                    .iter()
                    .find(|task| task.snapshot.id == id)
                    .ok_or("missing hosted process fact")?
                    .lifetime;
                t.snapshot.id = id;
                self.persist().await?;
                Ok(Reply::Observation(t))
            }
        }
    }
}
fn constant_time_equal(a: &str, b: &str) -> bool {
    let mut difference = a.len() ^ b.len();
    for (x, y) in a.bytes().zip(b.bytes()) {
        difference |= (x ^ y) as usize;
    }
    difference == 0
}
/// Run in the composition-root daemon process, never within an agent runtime.
pub async fn serve(config: ExecutionHostConfig) -> Result<(), String> {
    secure_dir(&config.state_dir)?;
    if config.state_dir.starts_with(&config.repo_root) {
        return Err("Execution Host state must be outside repository write authority".into());
    }
    let _owner = lock(&config.state_dir.join("host.lock"))?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let ready = Ready {
        address: listener.local_addr().map_err(|e| e.to_string())?,
        token: format!("{}{}", fresh_id()?, fresh_id()?),
        instance: fresh_id()?,
        pid: std::process::id(),
        repo: config.repo_root.clone(),
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
        capabilities: REQUIRED_CAPABILITIES
            .iter()
            .copied()
            .chain([OWNER_LOSS_CAPABILITY])
            .chain(if cfg!(unix) {
                vec![
                    "terminal.pty",
                    "terminal.input",
                    "terminal.resize",
                    "terminal.raw-observe",
                ]
            } else {
                vec![]
            })
            .map(|s| s.to_string())
            .collect(),
    };
    // Refuse crash adoption; a PID record is not ownership.
    let archive = match std::fs::read(config.state_dir.join("tasks.json")) {
        Ok(b) => serde_json::from_slice::<Vec<HostedTask>>(&b).map_err(|e| e.to_string())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.to_string()),
    };
    if archive.iter().any(|t| {
        matches!(
            t.snapshot.status,
            BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
        )
    }) {
        return Err("previous Execution Host lost with unmanaged tasks; start the runtime so it can reconcile them from OS evidence".into());
    }
    match std::fs::read(config.state_dir.join("intents.json")) {
        Ok(b) => {
            let intents: Vec<SpawnIntent> =
                serde_json::from_slice(&b).map_err(|e| e.to_string())?;
            if !intents.is_empty() {
                return Err("previous Execution Host has uncertain spawn intents; start the runtime so they are archived before a host starts".into());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("could not establish prior spawn state: {e}")),
    }
    let host = Arc::new(Host {
        registry: BackgroundTaskRegistry::new().retain_process_facts(),
        entries: Mutex::new(HashMap::new()),
        archive: Mutex::new(
            archive
                .into_iter()
                .map(|t| (t.snapshot.id.clone(), t))
                .collect(),
        ),
        acknowledged: Mutex::new(HashSet::new()),
        intents: Mutex::new(Vec::new()),
        shutdown: tokio_util::sync::CancellationToken::new(),
        retiring: std::sync::atomic::AtomicBool::new(false),
        persistence: Mutex::new(()),
        ready: ready.clone(),
        config,
    });
    // A supervisor's SIGTERM is a shutdown, not an abort. Bridging it onto the
    // same token as an idle exit means the ready file is removed on the way
    // out, so a stopped host leaves no endpoint for a later probe to trust.
    #[cfg(unix)]
    {
        let shutdown = host.shutdown.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut terminate), Ok(mut interrupt)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) else {
                return;
            };
            tokio::select! {
                _ = terminate.recv() => {}
                _ = interrupt.recv() => {}
            }
            shutdown.cancel();
        });
    }
    #[cfg(windows)]
    {
        let shutdown = host.shutdown.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                shutdown.cancel();
            }
        });
    }
    // Subscribe before readiness permits the first client spawn.
    let mut events = host.registry.subscribe();
    host.persist().await?;
    atomic_json(&host.config.state_dir.join("ready.json"), &ready)?;
    // Lifecycle boundaries (started/terminal) are durable immediately; a
    // chatty command's output is committed on a bounded cadence instead of one
    // atomic archive replacement per line. A crash between ticks can lose the
    // newest output tail, never the exit fact: every terminal observation goes
    // through `get_owned`, which forces a write before it is acknowledged.
    let persistence = host.clone();
    tokio::spawn(async move {
        const PERSIST_INTERVAL: Duration = Duration::from_millis(200);
        let mut ticker = tokio::time::interval(PERSIST_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dirty = false;
        loop {
            tokio::select! {
                _ = persistence.shutdown.cancelled() => break,
                _ = ticker.tick(), if dirty => {
                    if let Err(e) = persistence.persist().await {
                        tracing::error!("Execution Host persistence failed: {e}");
                    }
                    dirty = false;
                }
                event = events.recv() => match event {
                    Ok(event) => {
                        dirty = true;
                        let exited = matches!(&event, crate::BackgroundTaskEvent::Exited { .. });
                        if matches!(
                            &event,
                            crate::BackgroundTaskEvent::Started { .. }
                                | crate::BackgroundTaskEvent::Exited { .. }
                        ) {
                            #[cfg(debug_assertions)]
                            if exited {
                                debug_fault("after-exit-event");
                            }
                            if let Err(e) = persistence.persist().await {
                                tracing::error!("Execution Host persistence failed: {e}");
                            }
                            dirty = false;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => dirty = true,
                    Err(_) => break,
                },
            }
        }
    });
    let permits = Arc::new(tokio::sync::Semaphore::new(32));
    loop {
        let (mut stream, _) = tokio::select! {
            _=host.shutdown.cancelled()=>{std::fs::remove_file(host.config.state_dir.join("ready.json")).map_err(|e|e.to_string())?;return Ok(());},
            accepted=listener.accept()=>accepted.map_err(|e|e.to_string())?,
        };
        let permit = match permits.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let host = host.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let result =
                tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream)).await;
            let envelope = match result {
                Ok(Ok(b)) => serde_json::from_slice::<Envelope>(&b).ok(),
                _ => None,
            };
            let Some(envelope) = envelope else {
                return;
            };
            if !constant_time_equal(&envelope.token, &host.ready.token) {
                return;
            }
            let shutdown_requested = matches!(&envelope.request, Request::ShutdownIfIdle);
            let reply = if envelope.major != PROTOCOL_MAJOR
                && !SUPPORTED_LEGACY_MAJORS.contains(&envelope.major)
            {
                Reply::Error("Execution Host protocol incompatible; services preserved".into())
            } else if envelope.major < PROTOCOL_MAJOR && request_is_unrestricted(&envelope.request)
            {
                // The unrestricted control plane is a current-major capability.
                // A legacy caller is answered with the owner-scoped semantics it
                // actually declared, never upgraded to cross-owner control.
                Reply::Error(
                    "unrestricted control requires the current Execution Host protocol major"
                        .into(),
                )
            } else {
                match host.handle(envelope.request).await {
                    Ok(r) => r,
                    Err(e) => Reply::Error(e),
                }
            };
            let shutdown_accepted = shutdown_requested && matches!(&reply, Reply::Acknowledged);
            let _ = tokio::time::timeout(Duration::from_secs(5), write_frame(&mut stream, &reply))
                .await;
            if shutdown_accepted {
                host.shutdown.cancel();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_host_replay_owner_raw_cursor_and_generation_lifetime() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let config = ExecutionHostConfig {
            state_dir: root.path().join("host"),
            repo_root: repo.clone(),
            executable: PathBuf::from("unused-test-executable"),
        };
        let server = tokio::spawn(serve(config.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while ExecutionHostClient::connect(config.clone()).await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let environment = Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            repo.clone(),
            std::env::temp_dir(),
        ));
        let registry = BackgroundTaskRegistry::with_environment(environment.clone())
            .with_execution_host(config.clone());
        let id = format!("host-{}", fresh_id().unwrap());
        let mut request = ProcessRequest::new(
            "/bin/sh",
            vec![
                "-c".into(),
                "head -c 300000 /dev/zero; printf '\\377\\033[31mEND'; sleep 30".into(),
            ],
            repo,
        );
        request.unrestricted_execution = true;
        request.pty = Some(crate::command::PtySize { cols: 80, rows: 24 });
        let first = registry
            .create_terminal(&id, request.clone(), "owner")
            .await
            .unwrap();
        let pid = first.snapshot.pid.unwrap();
        assert!(process_alive(pid));
        let replay = registry
            .create_terminal(&id, request.clone(), "owner")
            .await
            .unwrap();
        assert_eq!(
            first.snapshot.pid, replay.snapshot.pid,
            "same terminal id must not spawn again"
        );
        assert!(
            registry
                .create_terminal(&id, request.clone(), "foreign")
                .await
                .is_err()
        );
        assert_eq!(
            crate::host_settlement::SettlementJournal::open(&config.semantic_dir())
                .unwrap()
                .read(&id)
                .unwrap()
                .owner,
            "owner"
        );
        let mut observation = registry
            .terminal_observe(&id, "owner", 0, 65536)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while observation.output_end < 300009 {
                tokio::task::yield_now().await;
                observation = registry
                    .terminal_observe(&id, "owner", 0, 65536)
                    .await
                    .unwrap();
            }
        })
        .await
        .unwrap();
        assert!(observation.truncated);
        assert_eq!(
            observation.output_end - observation.output_start,
            256 * 1024
        );
        let capped = registry
            .terminal_observe(&id, "owner", 0, 16)
            .await
            .unwrap();
        assert_eq!(capped.data.len(), 16);
        assert_eq!(capped.next_cursor, capped.output_start + 16);
        let tail = registry
            .terminal_observe(&id, "owner", observation.output_end - 9, 16)
            .await
            .unwrap();
        assert_eq!(tail.data, b"\xff\x1b[31mEND");
        assert!(!tail.truncated);
        assert!(tail.output_error.is_none());
        assert!(
            registry
                .terminal_observe(&id, "foreign", 0, 16)
                .await
                .is_err()
        );
        assert!(
            registry
                .terminal_observe(&id, "owner", observation.output_end + 1, 16)
                .await
                .is_err()
        );
        assert!(
            registry
                .terminal_observe(&id, "owner", 0, 65537)
                .await
                .is_err()
        );
        drop(registry);
        assert!(
            process_alive(pid),
            "terminal survives runtime generation detach"
        );
        let next =
            BackgroundTaskRegistry::with_environment(environment).with_execution_host(config);
        assert_eq!(
            next.terminal_observe(&id, "owner", tail.next_cursor, 16)
                .await
                .unwrap()
                .snapshot
                .pid,
            Some(pid)
        );
        next.kill_session("owner").await.unwrap();
        wait_until_dead(pid).await;
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unrestricted_host_task_control_crosses_sessions_without_owner_spoofing() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let config = ExecutionHostConfig {
            state_dir: root.path().join("host"),
            repo_root: repo.clone(),
            executable: PathBuf::from("unused"),
        };
        let server = tokio::spawn(serve(config.clone()));
        let client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(client) = ExecutionHostClient::connect(config.clone()).await {
                    break client;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let environment = Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            repo.clone(),
            std::env::temp_dir(),
        ));
        let registry = crate::BackgroundTaskRegistry::with_environment(environment)
            .with_execution_host(config);
        let id = registry
            .spawn_owned_with_lifetime(
                ProcessRequest::new(
                    "sh",
                    vec!["-c".into(), "printf admin-log; sleep 30".into()],
                    repo,
                ),
                None,
                Some("owner-session"),
                BackgroundTaskLifetime::Persistent,
            )
            .await
            .unwrap();
        assert!(
            registry
                .get_authorized(&id, "foreign", false)
                .await
                .is_err()
        );
        assert!(
            registry
                .kill_authorized(&id, "foreign", false)
                .await
                .is_err()
        );
        assert!(
            registry
                .observe_authorized(
                    &id,
                    "foreign",
                    false,
                    Some(0),
                    1024,
                    Duration::ZERO,
                    &tokio_util::sync::CancellationToken::new()
                )
                .await
                .is_err()
        );
        let snapshot = registry.get_authorized(&id, "foreign", true).await.unwrap();
        assert_eq!(snapshot.owner_scope.as_deref(), Some("owner-session"));
        assert_eq!(snapshot.lifetime, BackgroundTaskLifetime::Persistent);
        let observed = registry
            .observe_authorized(
                &id,
                "foreign",
                true,
                Some(0),
                1024,
                Duration::from_secs(2),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(observed.snapshot.log.contains("admin-log"));
        assert_eq!(
            observed.snapshot.lifetime,
            BackgroundTaskLifetime::Persistent
        );
        let stopped = registry
            .kill_authorized(&id, "foreign", true)
            .await
            .unwrap();
        assert_eq!(stopped.lifetime, BackgroundTaskLifetime::Persistent);
        let snapshot = registry
            .wait_authorized(
                &id,
                "foreign",
                true,
                Some(Duration::from_secs(5)),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            snapshot.status,
            BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
        ));
        let observed = registry
            .observe_authorized(
                &id,
                "foreign",
                true,
                Some(0),
                1024,
                Duration::ZERO,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(observed.snapshot.log.contains("admin-log"));
        registry
            .take_settlement_authorized(&id, "foreign", true)
            .await
            .unwrap();
        client.acknowledge_unrestricted(&id).await.unwrap();
        assert!(client.get_owned(&id, "foreign").await.is_err());
        assert_eq!(
            client
                .get_unrestricted(&id)
                .await
                .unwrap()
                .owner_scope
                .as_deref(),
            Some("owner-session")
        );
        client.shutdown_if_idle().await.unwrap();
        server.await.unwrap().unwrap();
    }

    use super::*;
    #[test]
    fn incompatible_major_and_missing_capability_are_rejected() {
        let capabilities = REQUIRED_CAPABILITIES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert!(check_compatibility(PROTOCOL_MAJOR, &capabilities).is_ok());
        assert!(check_compatibility(PROTOCOL_MAJOR + 1, &capabilities).is_err());
        assert!(check_compatibility(PROTOCOL_MAJOR, &[]).is_err());
        // A legacy host keeps the SAME owner-scoped control plane, so a newer
        // runtime must not strand the services it is still running.
        for legacy in SUPPORTED_LEGACY_MAJORS {
            assert!(
                check_compatibility(*legacy, &capabilities).is_ok(),
                "major {legacy} must stay controllable"
            );
            assert!(
                check_compatibility(*legacy, &[]).is_err(),
                "a legacy host without the control capabilities is refused"
            );
        }
    }
    #[test]
    fn bearer_token_comparison_rejects_partial_and_wrong_credentials() {
        assert!(constant_time_equal("abc", "abc"));
        assert!(!constant_time_equal("abc", "abd"));
        assert!(!constant_time_equal("abc", "ab"));
    }

    /// Owner loss is recorded, never enacted: the host still owns the process,
    /// and only the generation that admitted the work can declare that it has
    /// gone. A second declaration keeps the first observation, so repeated
    /// reconciliation cannot rewrite when the loss happened.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owner_loss_is_attributed_idempotent_and_never_signals_the_process() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let config = ExecutionHostConfig {
            state_dir: root.path().join("host"),
            repo_root: repo.clone(),
            executable: PathBuf::from("unused-test-executable"),
        };
        let server = tokio::spawn(serve(config.clone()));
        let client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(c) = ExecutionHostClient::connect(config.clone()).await {
                    break c;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(client.supports(OWNER_LOSS_CAPABILITY));
        let id = format!("host-{}", fresh_id().unwrap());
        let request = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                "while :; do printf tick; sleep 0.05; done".into(),
            ],
            repo,
        );
        let started = client
            .spawn(
                &id,
                request.clone(),
                "session",
                "parent",
                BackgroundTaskLifetime::Goal,
                crate::BackgroundTaskOrigin::default(),
                Some("boot-a"),
            )
            .await
            .unwrap();
        let pid = started.pid.expect("hosted goal task has a pid");
        let listed = || async {
            client
                .list()
                .await
                .unwrap()
                .into_iter()
                .find(|task| task.snapshot.id == id)
                .expect("hosted task is listed")
        };
        assert_eq!(listed().await.owner_boot.as_deref(), Some("boot-a"));
        assert!(listed().await.owner_loss.is_none());

        // Only the admitting owner, naming the admitting generation, may
        // declare loss; a wrong or empty boot is not evidence of anything.
        assert!(
            client
                .owner_lost(&id, "another-session", "boot-a")
                .await
                .is_err()
        );
        assert!(client.owner_lost(&id, "session", "boot-b").await.is_err());
        assert!(client.owner_lost(&id, "session", "").await.is_err());
        assert!(
            client
                .owner_lost("host-unknown", "session", "boot-a")
                .await
                .is_err()
        );
        assert!(listed().await.owner_loss.is_none());

        client.owner_lost(&id, "session", "boot-a").await.unwrap();
        let first = listed().await;
        let loss = first.owner_loss.clone().expect("loss is recorded");
        assert_eq!(loss.owner_boot, "boot-a");
        assert!(loss.observed_at_ms >= first.started_at_ms);
        assert!(process_alive(pid), "owner loss must not signal the process");
        let marked = client.get_owned(&id, "session").await.unwrap();
        assert_eq!(marked.status, BackgroundTaskStatus::Running);

        // Reconciliation may re-declare the same loss many times. The first
        // observation is the fact; later calls must not move it.
        client.owner_lost(&id, "session", "boot-a").await.unwrap();
        assert_eq!(
            listed().await.owner_loss.unwrap().observed_at_ms,
            loss.observed_at_ms
        );
        drop(client);
        let archive = HostRecords::read(&config.state_dir).unwrap();
        let archived = archive.task(&id).expect("archive keeps the record");
        assert_eq!(
            archived.owner_loss.as_ref().unwrap().observed_at_ms,
            loss.observed_at_ms,
            "the marker survives the client that wrote it"
        );
        assert!(process_alive(pid));

        let client = ExecutionHostClient::connect(config.clone()).await.unwrap();
        let persistent = format!("host-{}", fresh_id().unwrap());
        client
            .spawn(
                &persistent,
                request,
                "session",
                "parent",
                BackgroundTaskLifetime::Persistent,
                crate::BackgroundTaskOrigin::default(),
                None,
            )
            .await
            .unwrap();
        assert!(
            client
                .owner_lost(&persistent, "session", "boot-a")
                .await
                .is_err(),
            "only goal work belongs to a runtime generation"
        );
        client.kill_owned(&id, "session").await.unwrap();
        wait_until_dead(pid).await;
        client.kill_owned(&persistent, "session").await.unwrap();
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_process_survives_clients_and_preserves_offline_exit_and_logs() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let config = ExecutionHostConfig {
            state_dir: root.path().join("host"),
            repo_root: repo.clone(),
            executable: PathBuf::from("unused-test-executable"),
        };
        let server = tokio::spawn(serve(config.clone()));
        let client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(c) = ExecutionHostClient::connect(config.clone()).await {
                    break c;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let id = format!("host-{}", fresh_id().unwrap());
        let request = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "printf before; sleep 0.1; printf after".into()],
            repo.clone(),
        );
        client
            .spawn(
                &id,
                request,
                "session",
                "parent",
                BackgroundTaskLifetime::Persistent,
                crate::BackgroundTaskOrigin::default(),
                None,
            )
            .await
            .unwrap();
        drop(client);
        // This time interval is the property: exit and logs occur with no client.
        tokio::time::sleep(Duration::from_millis(750)).await;
        let client = ExecutionHostClient::connect(config.clone()).await.unwrap();
        let task = client.get_owned(&id, "session").await.unwrap();
        assert_eq!(task.status, BackgroundTaskStatus::Exited);
        assert_eq!(task.exit_code, Some(0));
        assert!(task.log.contains("before") && task.log.contains("after"));
        let facts = client.list().await.unwrap();
        let fact = facts.iter().find(|t| t.snapshot.id == id).unwrap();
        assert!(fact.process_id.is_some());
        assert!(fact.finished_at_ms.is_some());
        let finished = fact.finished_at_ms;
        assert_eq!(
            client
                .list()
                .await
                .unwrap()
                .iter()
                .find(|t| t.snapshot.id == id)
                .unwrap()
                .finished_at_ms,
            finished
        );
        let persistent = format!("host-{}", fresh_id().unwrap());
        let request = ProcessRequest::new(
            "sh",
            vec![
                "-c".into(),
                "while :; do printf tick; sleep 0.05; done".into(),
            ],
            repo,
        );
        client
            .spawn(
                &persistent,
                request.clone(),
                "session",
                "parent",
                BackgroundTaskLifetime::Persistent,
                crate::BackgroundTaskOrigin::default(),
                None,
            )
            .await
            .unwrap();
        let original = client
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.snapshot.id == persistent)
            .unwrap()
            .process_id;
        let second = ExecutionHostClient::connect(config.clone()).await.unwrap();
        second
            .spawn(
                &persistent,
                request,
                "session",
                "parent",
                BackgroundTaskLifetime::Persistent,
                crate::BackgroundTaskOrigin::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            second
                .list()
                .await
                .unwrap()
                .into_iter()
                .find(|t| t.snapshot.id == persistent)
                .unwrap()
                .process_id,
            original
        );
        assert!(
            second
                .kill_owned(&persistent, "another-session")
                .await
                .is_err()
        );
        let mut incompatible = config.clone();
        incompatible.state_dir = root.path().join("incompatible");
        secure_dir(&incompatible.state_dir).unwrap();
        let mut wrong = ready(&config).unwrap();
        wrong.major += 1;
        atomic_json(&incompatible.state_dir.join("ready.json"), &wrong).unwrap();
        assert!(
            ExecutionHostClient::connect(incompatible.clone())
                .await
                .is_err()
        );
        wrong.major = PROTOCOL_MAJOR;
        wrong.minor += 7;
        atomic_json(&incompatible.state_dir.join("ready.json"), &wrong).unwrap();
        assert!(
            ExecutionHostClient::connect(incompatible.clone())
                .await
                .is_ok()
        );
        wrong.capabilities.clear();
        atomic_json(&incompatible.state_dir.join("ready.json"), &wrong).unwrap();
        assert!(ExecutionHostClient::connect(incompatible).await.is_err());
        // Real owner lock: incompatible discovery must never replace this host.
        let original_ready = ready(&config).unwrap();
        let mut incompatible_live = original_ready.clone();
        incompatible_live.major += 1;
        atomic_json(&config.state_dir.join("ready.json"), &incompatible_live).unwrap();
        assert!(ExecutionHostClient::ensure(config.clone()).await.is_err());
        assert_eq!(
            client
                .list()
                .await
                .unwrap()
                .into_iter()
                .find(|t| t.snapshot.id == persistent)
                .unwrap()
                .process_id,
            original
        );
        incompatible_live.major = PROTOCOL_MAJOR;
        incompatible_live.capabilities.clear();
        atomic_json(&config.state_dir.join("ready.json"), &incompatible_live).unwrap();
        assert!(ExecutionHostClient::ensure(config.clone()).await.is_err());
        assert_eq!(
            client
                .get_owned(&persistent, "session")
                .await
                .unwrap()
                .status,
            BackgroundTaskStatus::Running
        );
        atomic_json(&config.state_dir.join("ready.json"), &original_ready).unwrap();
        let changed = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "exit 99".into()],
            config.repo_root.clone(),
        );
        assert!(
            second
                .spawn(
                    &persistent,
                    changed,
                    "session",
                    "parent",
                    BackgroundTaskLifetime::Persistent,
                    crate::BackgroundTaskOrigin::default(),
                    None,
                )
                .await
                .is_err()
        );
        assert_eq!(
            client
                .get_owned(&persistent, "session")
                .await
                .unwrap()
                .status,
            BackgroundTaskStatus::Running
        );
        assert!(client.shutdown_if_idle().await.is_err());
        // Writer retirement must not silently stop an explicit persistent service.
        let registry = BackgroundTaskRegistry::new().with_execution_host(config.clone());
        let request = ProcessRequest::new(
            "sh",
            vec!["-c".into(), "sleep 30".into()],
            config.repo_root.clone(),
        );
        let owned = registry
            .spawn_for_writer(
                request,
                None,
                Some("writer-session"),
                "child",
                BackgroundTaskLifetime::Persistent,
                None,
            )
            .await
            .unwrap();
        registry
            .settle_writer("writer-session", "child")
            .await
            .unwrap();
        assert_eq!(
            registry
                .get_owned(&owned, "writer-session")
                .await
                .unwrap()
                .status,
            BackgroundTaskStatus::Running
        );
        assert_eq!(
            registry
                .foreign_write_paths("writer-session", "another-writer")
                .await,
            vec!["."]
        );
        assert!(
            registry
                .write_conflicts("writer-session", &["file.txt".into()])
                .await
                .contains(&owned)
        );
        registry.kill_owned(&owned, "writer-session").await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if registry
                    .get_owned(&owned, "writer-session")
                    .await
                    .is_ok_and(|t| {
                        matches!(
                            t.status,
                            BackgroundTaskStatus::Killed | BackgroundTaskStatus::Exited
                        )
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            registry
                .foreign_write_paths("writer-session", "another-writer")
                .await
                .is_empty()
        );
        client.kill_owned(&persistent, "session").await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if matches!(
                    client
                        .get_owned(&persistent, "session")
                        .await
                        .unwrap()
                        .status,
                    BackgroundTaskStatus::Killed | BackgroundTaskStatus::Exited
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // Nine full retained logs exceed a single frame unless inventory is metadata-only.
        let mut large_ids = Vec::new();
        for _ in 0..9 {
            let id = format!("host-{}", fresh_id().unwrap());
            let request = ProcessRequest::new(
                "sh",
                vec!["-c".into(), "head -c 280000 /dev/zero | tr '\\0' x".into()],
                config.repo_root.clone(),
            );
            client
                .spawn(
                    &id,
                    request,
                    "logs-session",
                    "parent",
                    BackgroundTaskLifetime::Persistent,
                    crate::BackgroundTaskOrigin::default(),
                    None,
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if matches!(
                        client.get_owned(&id, "logs-session").await.unwrap().status,
                        BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
                    ) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            large_ids.push(id);
        }
        let inventory = client.list().await.unwrap();
        assert!(inventory.iter().all(|t| t.snapshot.log.is_empty()));
        let bytes = serde_json::to_vec(&inventory).unwrap().len();
        assert!(bytes < FRAME_LIMIT);
        for id in large_ids {
            assert!(
                client
                    .get_owned(&id, "logs-session")
                    .await
                    .unwrap()
                    .log
                    .len()
                    > 200000
            );
        }
        client.shutdown_if_idle().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    /// A major-1 host predates the `Runtime` owner tag. The same contract is
    /// spelled `Persistent` there, and that is what the wire must carry; the
    /// current major keeps `Runtime` distinct.
    #[test]
    fn a_legacy_host_is_told_persistent_for_a_runtime_service() {
        use BackgroundTaskLifetime::*;
        assert_eq!(
            hosted_wire_lifetime(SUPPORTED_LEGACY_MAJORS[0], Runtime),
            Persistent
        );
        assert_eq!(
            hosted_wire_lifetime(SUPPORTED_LEGACY_MAJORS[0], Session),
            Session
        );
        assert_eq!(
            hosted_wire_lifetime(SUPPORTED_LEGACY_MAJORS[0], Persistent),
            Persistent
        );
        assert_eq!(hosted_wire_lifetime(PROTOCOL_MAJOR, Runtime), Runtime);
    }

    /// The major-1 and major-2 control messages are the same shape apart from
    /// the `unrestricted` flag, so the two majors interoperate on the
    /// owner-scoped control plane in both directions.
    #[test]
    fn legacy_and_current_control_messages_interoperate() {
        // A major-1 request (no `unrestricted` field) is read by this build.
        let legacy: Request =
            serde_json::from_str(r#"{"Get":{"id":"host-1","owner":"session"}}"#).unwrap();
        match legacy {
            Request::Get {
                id,
                owner,
                unrestricted,
            } => {
                assert_eq!(id, "host-1");
                assert_eq!(owner, "session");
                assert!(!unrestricted, "a missing flag is never an escalation");
            }
            _ => panic!("expected Get"),
        }
        // This build's owner-scoped request is exactly what a major-1 host
        // already understands: the extra flag is additive.
        let current = serde_json::to_value(Request::Acknowledge {
            id: "host-1".into(),
            owner: "session".into(),
            unrestricted: false,
        })
        .unwrap();
        assert_eq!(current["Acknowledge"]["id"], "host-1");
        assert_eq!(current["Acknowledge"]["owner"], "session");
    }

    #[cfg(unix)]
    fn process_alive(pid: u32) -> bool {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        kill(Pid::from_raw(pid as i32), None).is_ok()
    }

    #[cfg(unix)]
    async fn wait_until_dead(pid: u32) {
        for _ in 0..200 {
            if !process_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("process {pid} was still alive after the stop");
    }

    /// A `Runtime`-lifetime service is the documented lifetime for a
    /// user-requested dev server or watcher. It belongs to the persistent
    /// execution substrate, not to the generation that launched it, so it must
    /// not count as retiring runtime work, must survive the launching registry
    /// being dropped, and must stay inspectable and stoppable from the next
    /// generation.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_lifetime_service_survives_generation_replacement() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let config = ExecutionHostConfig {
            state_dir: root.path().join("host"),
            repo_root: repo.clone(),
            executable: PathBuf::from("unused-test-executable"),
        };
        let server = tokio::spawn(serve(config.clone()));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if ExecutionHostClient::connect(config.clone()).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let environment = Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            repo.clone(),
            std::env::temp_dir(),
        ));

        // Generation 1 launches the service.
        let generation_1 = crate::BackgroundTaskRegistry::with_environment(environment.clone())
            .with_execution_host(config.clone());
        let id = generation_1
            .spawn_for_writer(
                ProcessRequest::new(
                    "sh",
                    vec!["-c".into(), "printf service-up; sleep 30".into()],
                    repo.clone(),
                ),
                None,
                Some("session-1"),
                "parent",
                BackgroundTaskLifetime::Runtime,
                None,
            )
            .await
            .unwrap();
        assert!(
            id.starts_with("host-"),
            "a Runtime service must be owned by the persistent substrate: {id}"
        );
        let launched = generation_1.get_owned(&id, "session-1").await.unwrap();
        let pid = launched.pid.expect("a running service has a pid");
        assert!(launched.log.contains("service-up"), "{}", launched.log);
        assert!(process_alive(pid));
        // Nothing about this service is retiring runtime work.
        assert!(
            generation_1.try_update_blockers().await.unwrap().is_empty(),
            "a hosted service must not block a generation handover"
        );

        // The launching generation disappears without taking the service with it.
        drop(generation_1);
        assert!(
            process_alive(pid),
            "the service must outlive the generation that launched it"
        );

        // The next generation re-attaches and keeps full control.
        let generation_2 = crate::BackgroundTaskRegistry::with_environment(environment)
            .with_execution_host(config.clone());
        generation_2.reconcile_hosted().await.unwrap();
        let reattached = generation_2.get_owned(&id, "session-1").await.unwrap();
        assert_eq!(reattached.status, BackgroundTaskStatus::Running);
        assert!(reattached.log.contains("service-up"));
        assert!(
            generation_2
                .try_all_snapshots()
                .await
                .unwrap()
                .iter()
                .any(|task| task.id == id),
            "the replacement generation must see the live service"
        );
        generation_2.kill_owned(&id, "session-1").await.unwrap();
        wait_until_dead(pid).await;
        // The signal and the durable settlement are separate steps: the process
        // is gone before the task reaches a terminal status, so read it once it
        // has settled rather than once right after the process dies.
        let mut stopped = generation_2.get_owned(&id, "session-1").await.unwrap();
        for _ in 0..200 {
            if matches!(
                stopped.status,
                BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            stopped = generation_2.get_owned(&id, "session-1").await.unwrap();
        }
        assert!(
            matches!(
                stopped.status,
                BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
            ),
            "stopping the service must settle it: {:?}",
            stopped.status
        );
        server.abort();
    }

    /// The reported blocker shape: the root process exits (even non-zero) while
    /// a descendant keeps the inherited log pipes open, so the task stays
    /// unsettled. That is real live work — it must stay inspectable — but it is
    /// owned by the persistent substrate, so it must NOT block a generation
    /// handover the way the same shape once did while it was runtime-local.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hosted_task_with_an_exited_root_does_not_block_handover() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let config = ExecutionHostConfig {
            state_dir: root.path().join("host"),
            repo_root: repo.clone(),
            executable: PathBuf::from("unused-test-executable"),
        };
        let server = tokio::spawn(serve(config.clone()));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if ExecutionHostClient::connect(config.clone()).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let environment = Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            repo.clone(),
            std::env::temp_dir(),
        ));
        let generation = crate::BackgroundTaskRegistry::with_environment(environment)
            .with_execution_host(config.clone());
        let id = generation
            .spawn_for_writer(
                ProcessRequest::new(
                    "sh",
                    vec![
                        "-c".into(),
                        // The root `sh` exits 2 at once; the backgrounded `sleep`
                        // inherits its stdout/stderr and keeps them open.
                        "sleep 30 & printf root-done; exit 2".into(),
                    ],
                    repo.clone(),
                ),
                None,
                Some("session-1"),
                "parent",
                BackgroundTaskLifetime::Runtime,
                None,
            )
            .await
            .unwrap();

        let mut exited_root = None;
        for _ in 0..200 {
            let snapshot = generation.get_owned(&id, "session-1").await.unwrap();
            if snapshot.exit_code.is_some() {
                exited_root = Some(snapshot);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let snapshot = exited_root.expect("the root must report its exit code");
        assert_eq!(snapshot.exit_code, Some(2));
        assert_eq!(
            snapshot.status,
            BackgroundTaskStatus::Running,
            "a live descendant keeps the workload unsettled"
        );
        assert!(snapshot.log.contains("root-done"), "{}", snapshot.log);
        // Unsettled live work, but not retiring runtime work: the handover waits
        // on the runtime generation's own work, not on the persistent substrate.
        assert!(
            generation.try_update_blockers().await.unwrap().is_empty(),
            "an unsettled hosted task must not block the handover"
        );
        generation.kill_owned(&id, "session-1").await.unwrap();
        server.abort();
    }

    /// A host that dies before readiness is otherwise invisible: its stderr
    /// was sent to the null device, so the failure named only an exit status.
    /// The diagnostic must carry the host's own last words.
    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_failure_reports_the_host_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let executable = root.path().join("fake-host");
        std::fs::write(
            &executable,
            "#!/bin/sh\necho 'refusing to start: state directory is not writable' >&2\nexit 7\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = ExecutionHostConfig {
            state_dir: root.path().join("host"),
            repo_root: repo,
            executable,
        };

        let error = match ExecutionHostClient::ensure(config).await {
            Ok(_) => panic!("a host that exits before readiness must not be adopted"),
            Err(error) => error,
        };
        assert!(
            error.contains("exited before readiness"),
            "the readiness failure keeps its own identity: {error}"
        );
        assert!(
            error.contains("refusing to start: state directory is not writable"),
            "the host's stderr is the evidence: {error}"
        );
    }

    #[cfg(unix)]
    fn host_config(root: &Path) -> ExecutionHostConfig {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        ExecutionHostConfig {
            state_dir: root.join("host"),
            repo_root: repo,
            executable: PathBuf::from("unused-test-executable"),
        }
    }

    /// A ready file is a *derived* fact: it names an endpoint, never an owner.
    /// Whether it still describes anything is decided by the lifecycle lock,
    /// which the kernel releases when its holder dies.
    #[cfg(unix)]
    fn write_ready_residue(config: &ExecutionHostConfig, address: &str, major: u32) {
        secure_dir(&config.state_dir).unwrap();
        let ready = Ready {
            address: address.parse().unwrap(),
            token: "0".repeat(64),
            instance: "residue-instance".into(),
            pid: 1,
            repo: config.repo_root.clone(),
            major,
            minor: PROTOCOL_MINOR,
            capabilities: REQUIRED_CAPABILITIES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        atomic_json(&config.state_dir.join("ready.json"), &ready).unwrap();
    }

    #[cfg(unix)]
    fn task_record(
        config: &ExecutionHostConfig,
        id: &str,
        status: &str,
        pid: u32,
    ) -> serde_json::Value {
        serde_json::json!({
            "snapshot": {
                "id": id, "program": "sleep", "args": ["30"],
                "cwd": config.repo_root.clone(), "status": status, "exit_code": null,
                "log": "", "duration_ms": 0, "pid": pid, "owner_scope": "session"
            },
            "lifetime": "Persistent",
            "writer_scope": "parent",
            "write_scope": "Unrestricted",
            "process_id": pid,
            "started_at_ms": 0,
            "finished_at_ms": null,
            "log_end": 0,
            "log_prefix_len": 0
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn archived_lifetime_uses_the_host_owned_contract() {
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        secure_dir(&config.state_dir).unwrap();
        let mut record = task_record(&config, "host-archived", "Exited", 0);
        record["snapshot"]["lifetime"] = serde_json::json!("Runtime");
        atomic_json(&config.state_dir.join("tasks.json"), &vec![record]).unwrap();
        let server = tokio::spawn(serve(config.clone()));
        let client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(client) = ExecutionHostClient::connect(config.clone()).await {
                    break client;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let snapshot = client.get_owned("host-archived", "session").await.unwrap();
        server.abort();
        assert_eq!(snapshot.status, BackgroundTaskStatus::Exited);
        assert_eq!(snapshot.lifetime, BackgroundTaskLifetime::Persistent);
    }

    #[cfg(unix)]
    fn write_archive(config: &ExecutionHostConfig, status: &str, pid: u32) {
        write_archive_tasks(config, &[("host-recorded", status, pid)]);
    }

    #[cfg(unix)]
    fn write_archive_tasks(config: &ExecutionHostConfig, tasks: &[(&str, &str, u32)]) {
        let tasks = tasks
            .iter()
            .map(|(id, status, pid)| task_record(config, id, status, *pid))
            .collect::<Vec<_>>();
        std::fs::write(
            config.state_dir.join("tasks.json"),
            serde_json::to_vec(&tasks).unwrap(),
        )
        .unwrap();
    }

    /// A process group that is gone, represented by the pid of a real child that
    /// has been spawned as a group leader and reaped.
    #[cfg(unix)]
    fn exited_group_leader() -> u32 {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn group leader");
        let pid = child.id();
        child.wait().expect("reap group leader");
        pid
    }

    /// A refused endpoint plus a free lifecycle lock plus settled records is a
    /// dead host's residue. Reading it as "no host" is what lets a runtime start
    /// again; nothing here is an owner decision.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_clears_ready_residue_left_by_a_host_that_owns_no_work() {
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR);
        let probed = ExecutionHostClient::probe(&config)
            .await
            .expect("settled residue must not surface an error");
        assert!(probed.client.is_none(), "no host owns this endpoint");
        assert!(
            probed
                .recovery
                .is_none_or(|recovery| recovery.tasks.is_empty()),
            "there is nothing to recover"
        );
        assert!(
            !config.state_dir.join("ready.json").exists(),
            "residue of a provably dead host is not state"
        );
    }

    /// A record that never committed a completion has no terminal exit fact.
    /// A partially observed code/signal must not survive beside `Unknown`.
    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_clears_a_partial_exit_fact_it_did_not_commit() {
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR);
        let pid = exited_group_leader();
        write_archive_tasks(&config, &[("host-partial", "Running", pid)]);
        let path = config.state_dir.join("tasks.json");
        let mut records: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        records[0]["snapshot"]["exit_code"] = serde_json::json!(3);
        records[0]["snapshot"]["signal"] = serde_json::json!(15);
        records[0]["result"] = serde_json::json!({
            "result_id": "fabricated", "status": "Exited", "exit_code": 3,
            "output_bytes": 0, "output_truncated": false, "completed_at_ms": 1
        });
        std::fs::write(&path, serde_json::to_vec(&records).unwrap()).unwrap();

        let probed = ExecutionHostClient::probe(&config).await.unwrap();
        assert!(
            probed.recovery.is_some(),
            "the running record must converge"
        );
        let recovered: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(recovered[0]["snapshot"]["status"], "Unknown");
        assert!(
            recovered[0]["snapshot"]["exit_code"].is_null(),
            "an uncommitted exit code must not survive: {recovered:?}"
        );
        assert!(
            recovered[0]["snapshot"]["signal"].is_null(),
            "an uncommitted signal must not survive: {recovered:?}"
        );
        assert!(
            recovered[0]["result"].is_null(),
            "a completion that was never committed must not survive: {recovered:?}"
        );
        assert_eq!(recovered[0]["loss"]["previous"], "Running");
        assert_eq!(recovered[0]["loss"]["process"], "Gone");
    }

    /// Two readers can race the same recovery pass. A lock this reader cannot
    /// take means another owner is converging the same residue, not that the
    /// host is unreachable; every caller must still get a durable answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_probes_of_a_lost_host_all_converge() {
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR);
        write_archive_tasks(&config, &[("host-raced", "Running", exited_group_leader())]);

        let mut readers = Vec::new();
        for _ in 0..8 {
            let config = config.clone();
            readers.push(tokio::spawn(async move {
                ExecutionHostClient::probe(&config).await
            }));
        }
        let mut recovered = 0;
        for reader in readers {
            let probe = reader
                .await
                .unwrap()
                .expect("a concurrent recovery pass is not a read failure");
            assert!(probe.client.is_none());
            recovered += usize::from(probe.recovery.is_some());
        }
        assert!(
            recovered >= 1,
            "at least one reader must converge the residue"
        );
        let records: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(config.state_dir.join("tasks.json")).unwrap())
                .unwrap();
        assert_eq!(records[0]["snapshot"]["status"], "Unknown");
        assert!(!config.state_dir.join("ready.json").exists());
    }

    /// Boot recovery is linear in the number of historical records and stays
    /// bounded at the scale a long-lived repository actually reaches.
    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_of_a_hundred_historical_records_stays_bounded() {
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR);
        let records: Vec<serde_json::Value> = (0..100)
            .map(|index| {
                let id = format!("host-history-{index}");
                let mut record = task_record(&config, &id, "Running", exited_group_leader());
                record["started_at_ms"] = serde_json::json!(index);
                record
            })
            .collect();
        std::fs::write(
            config.state_dir.join("tasks.json"),
            serde_json::to_vec(&records).unwrap(),
        )
        .unwrap();

        let started = std::time::Instant::now();
        let probed = ExecutionHostClient::probe(&config).await.unwrap();
        let elapsed = started.elapsed();
        let recovery = probed.recovery.expect("the residue must be converged");
        assert_eq!(recovery.tasks.len(), 100);
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "100 historical records took {elapsed:?}"
        );
        let recovered: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(config.state_dir.join("tasks.json")).unwrap())
                .unwrap();
        assert!(
            recovered
                .iter()
                .all(|record| record["snapshot"]["status"] == "Unknown"),
            "every unproven historical record must converge"
        );
        eprintln!("PERF recovery_100_records_ms={}", elapsed.as_millis());
    }

    /// Each record is judged on its own evidence: one finished workload and one
    /// that may still be alive must not get the same status or reservation.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_converges_each_task_from_its_own_evidence() {
        use std::os::unix::process::CommandExt;
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR);
        let mut live = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn live group");
        let live_pid = live.id();
        write_archive_tasks(
            &config,
            &[
                ("host-finished", "Running", exited_group_leader()),
                ("host-live", "Running", live_pid),
            ],
        );

        let probed = ExecutionHostClient::probe(&config).await.unwrap();
        let recovery = probed.recovery.expect("both records are converged");
        assert_eq!(recovery.tasks.len(), 2);
        let records = HostRecords::read(&config.state_dir).unwrap();
        let finished = records.task("host-finished").unwrap();
        let running = records.task("host-live").unwrap();
        assert_eq!(
            finished.loss.as_ref().unwrap().process,
            ProcessPresence::Gone
        );
        assert!(finished.settlement_proven());
        assert_eq!(
            running.loss.as_ref().unwrap().process,
            ProcessPresence::Present
        );
        assert!(!running.settlement_proven());
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(live_pid as i32), None).is_ok(),
            "the live workload is untouched"
        );
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(live_pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = live.wait();
    }

    /// A live record whose process group is gone is converged from OS evidence.
    /// It becomes an unobserved terminal fact, never a fabricated exit, and the
    /// product can start again without deleting anything.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_converges_a_running_record_whose_process_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR);
        let pid = exited_group_leader();
        write_archive(&config, "Running", pid);

        let probed = ExecutionHostClient::probe(&config)
            .await
            .expect("residue must not block startup forever");
        assert!(probed.client.is_none());
        let recovery = probed.recovery.expect("the pass reports what it converged");
        assert_eq!(recovery.tasks.len(), 1);
        assert_eq!(recovery.tasks[0].process, ProcessPresence::Gone);
        assert_eq!(recovery.tasks[0].previous, BackgroundTaskStatus::Running);
        assert!(!config.state_dir.join("ready.json").exists());

        let records = HostRecords::read(&config.state_dir).unwrap();
        let task = records.task("host-recorded").expect("the record is kept");
        assert_eq!(task.snapshot.status, BackgroundTaskStatus::Unknown);
        assert_eq!(task.snapshot.pid, Some(pid), "the OS pid stays as evidence");
        let loss = task.loss.as_ref().expect("the loss is explained");
        assert_eq!(loss.previous, BackgroundTaskStatus::Running);
        assert_eq!(loss.process, ProcessPresence::Gone);
        assert!(task.finished_at_ms.is_some());
        assert!(task.settlement_proven());
        assert!(
            task.result.is_none(),
            "a lost host never observed an exit, so it cannot have a completion record"
        );
    }

    /// A completion record is derived only from an exit this host observed.
    /// Recovery must delete one that a crash could not have proven, and the
    /// digest must be stable across equal reads but change with the fact.
    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_drops_a_completion_it_cannot_prove() {
        use std::os::unix::process::CommandExt;
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR);
        let mut live = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn live group");
        let live_pid = live.id();
        let mut record = task_record(&config, "host-fabricated", "Running", live_pid);
        // A bogus start token is exactly the shape a reused pid presents. It
        // must never authorize a signal, and the fabricated completion must be
        // discarded because this host never observed this task exit.
        record["snapshot"]["start_identity"] = serde_json::json!("not-the-current-process");
        record["result"] = serde_json::json!({
            "result_id": "fabricated",
            "status": "Exited",
            "exit_code": 0,
            "output_bytes": 0,
            "output_truncated": false,
            "completed_at_ms": 1
        });
        atomic_json(&config.state_dir.join("tasks.json"), &vec![record]).unwrap();

        let probed = ExecutionHostClient::probe(&config).await.unwrap();
        assert!(probed.client.is_none());
        let recovery = probed.recovery.expect("the live record is converged");
        assert_eq!(recovery.tasks.len(), 1);
        assert_eq!(recovery.tasks[0].process, ProcessPresence::Present);
        let records = HostRecords::read(&config.state_dir).unwrap();
        let task = records.task("host-fabricated").unwrap();
        assert_eq!(task.snapshot.status, BackgroundTaskStatus::Unknown);
        assert!(
            task.result.is_none(),
            "recovery cannot keep an exit it did not see"
        );
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(live_pid as i32), None).is_ok(),
            "a mismatched start token never authorizes a signal"
        );
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(live_pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = live.wait();
    }

    /// The result identity is content-bound and stable: a re-read of the same
    /// terminal fact yields the same id, while any change to the fact does not.
    #[test]
    fn a_completion_result_identity_is_stable_and_content_bound() {
        let snapshot: BackgroundTaskSnapshot = serde_json::from_value(serde_json::json!({
            "id": "host-result",
            "program": "sh",
            "args": ["-c", "exit 7"],
            "cwd": "/tmp",
            "status": "Exited",
            "exit_code": 7,
            "log": "[stdout] COMPLETED\n",
            "duration_ms": 10,
            "owner_scope": "session",
            "output_bytes": 20
        }))
        .unwrap();
        let first = CompletionRecord::of("host-result", &snapshot, 100);
        let repeated = CompletionRecord::of("host-result", &snapshot, 100);
        assert_eq!(first, repeated, "the same fact must keep the same identity");
        let mut changed = snapshot.clone();
        changed.log.push_str("later");
        assert_ne!(
            first.result_id,
            CompletionRecord::of("host-result", &changed, 100).result_id,
            "different output is a different result"
        );
        assert_ne!(
            first.result_id,
            CompletionRecord::of("host-other", &snapshot, 100).result_id,
            "the task identity is part of the result"
        );
    }

    /// A record whose process group still exists is not adopted, not signalled
    /// and not replayed. Control is recorded as lost, and the workload stays
    /// alive for its owner to handle.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_never_claims_or_kills_a_process_group_that_still_exists() {
        use std::os::unix::process::CommandExt;
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR);
        let mut live = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn live group");
        let pid = live.id();
        write_archive(&config, "Killing", pid);

        let probed = ExecutionHostClient::probe(&config)
            .await
            .expect("a live process is evidence, not a startup error");
        let recovery = probed.recovery.expect("the pass reports what it converged");
        assert_eq!(recovery.tasks[0].process, ProcessPresence::Present);
        let records = HostRecords::read(&config.state_dir).unwrap();
        let task = records.task("host-recorded").unwrap();
        assert_eq!(task.snapshot.status, BackgroundTaskStatus::Unknown);
        assert_eq!(
            task.loss.as_ref().unwrap().previous,
            BackgroundTaskStatus::Killing
        );
        assert!(!task.settlement_proven(), "a live process is not settled");
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok(),
            "recovery must not signal a process it cannot prove it owns"
        );

        // A second pass has nothing live to converge: recovery is idempotent.
        let again = ExecutionHostClient::probe(&config).await.unwrap();
        assert!(again.recovery.is_none(), "nothing left to converge");

        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = live.wait();
    }

    /// An intent persisted before an OS spawn leaves the same uncertainty as a
    /// live process: the host may have launched something it never recorded. It
    /// is archived as evidence and blocks nothing, because no host can ever
    /// resolve it and discarding it would delete the only trace.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_archives_a_spawn_intent_no_host_can_resolve() {
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR);
        std::fs::write(
            config.state_dir.join("intents.json"),
            br#"[{"id":"host-unresolved","owner":"session","program":"sh","args":["-c","true"],"cwd":"/tmp","started_at_ms":0}]"#,
        )
        .unwrap();

        let probed = ExecutionHostClient::probe(&config)
            .await
            .expect("an unprovable spawn must not block startup forever");
        let recovery = probed.recovery.expect("the pass reports the intent");
        assert_eq!(recovery.unresolved_intents, vec!["host-unresolved"]);
        assert!(!config.state_dir.join("ready.json").exists());

        let records = HostRecords::read(&config.state_dir).unwrap();
        assert!(records.has_unresolved_intent("host-unresolved"));
        assert!(!records.has_task("host-unresolved"));
        assert!(!records.intents_outstanding());
        assert_eq!(
            serde_json::from_slice::<Vec<serde_json::Value>>(
                &std::fs::read(config.state_dir.join("intents.json")).unwrap()
            )
            .unwrap()
            .len(),
            0
        );
        let archived: Vec<serde_json::Value> = serde_json::from_slice(
            &std::fs::read(config.state_dir.join("intents.unresolved.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(archived.len(), 1, "the intent's evidence is kept");
        assert_eq!(archived[0]["intent"]["program"], "sh");

        // Idempotent: a second pass cannot duplicate the archived evidence.
        let again = ExecutionHostClient::probe(&config).await.unwrap();
        assert!(again.recovery.is_none());
        let archived: Vec<serde_json::Value> = serde_json::from_slice(
            &std::fs::read(config.state_dir.join("intents.unresolved.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(archived.len(), 1);
    }

    /// An incompatible ready file is a live host's own metadata: the error the
    /// product reports must survive untouched.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_reports_an_incompatible_ready_file_without_cleaning_it() {
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        write_ready_residue(&config, "127.0.0.1:1", PROTOCOL_MAJOR + 1);
        let error = match ExecutionHostClient::probe(&config).await {
            Ok(_) => panic!("an unknown major is refused"),
            Err(error) => error,
        };
        assert!(error.contains("incompatible"), "{error}");
        assert!(config.state_dir.join("ready.json").exists());
    }

    /// The endpoint is the fast path, not the authority: a live host answers and
    /// is reused, and only a host the kernel proves gone is recovered.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_reuses_a_live_host_and_recovers_only_after_it_dies() {
        let root = tempfile::tempdir().unwrap();
        let config = host_config(root.path());
        let server = tokio::spawn(serve(config.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while ExecutionHostClient::connect(config.clone()).await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let live = ExecutionHostClient::probe(&config).await.unwrap();
        assert!(live.client.is_some(), "a live host must be reused");
        assert!(live.recovery.is_none(), "there is no residue to recover");

        // A crash: the holder dies without removing its endpoint. The kernel
        // drops the lifecycle lock with it; the endpoint then refuses.
        server.abort();
        let _ = server.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while ExecutionHostClient::connect(config.clone()).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let after = ExecutionHostClient::probe(&config).await.unwrap();
        assert!(after.client.is_none());
        assert!(after.recovery.is_some(), "the dead host's residue is read");
        assert!(!config.state_dir.join("ready.json").exists());
    }
}
