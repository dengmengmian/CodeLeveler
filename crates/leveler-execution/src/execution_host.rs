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
pub const PROTOCOL_MINOR: u32 = 0;
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
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let f = options.open(path).map_err(|e| e.to_string())?;
    f.try_lock_exclusive()
        .map_err(|_| "Execution Host lifecycle lock is busy".to_string())?;
    Ok(f)
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

#[derive(Clone)]
pub struct ExecutionHostClient {
    config: ExecutionHostConfig,
    ready: Ready,
    /// The major the connected host declared. A legacy host has no
    /// `unrestricted` control plane, so that path stays refused rather than
    /// silently degrading to an owner-scoped permission check.
    peer_major: u32,
}
impl ExecutionHostClient {
    pub async fn probe(config: &ExecutionHostConfig) -> Result<Option<Self>, String> {
        if !config.state_dir.join("ready.json").exists() {
            return Ok(None);
        }
        Self::connect(config.clone()).await.map(Some)
    }
    pub async fn connect(config: ExecutionHostConfig) -> Result<Self, String> {
        let r = ready(&config)?;
        let c = Self {
            peer_major: r.major,
            config,
            ready: r,
        };
        match c.call(Request::Hello).await? {
            Reply::Hello {
                major,
                capabilities,
                instance,
                ..
            } => {
                check_compatibility(major, &capabilities)?;
                if instance != c.ready.instance {
                    return Err("Execution Host instance changed".into());
                }
                if major != c.peer_major {
                    return Err("Execution Host major changed since readiness".into());
                }
                Ok(c)
            }
            _ => Err("invalid Execution Host handshake".into()),
        }
    }

    /// The major the connected host speaks.
    pub fn peer_major(&self) -> u32 {
        self.peer_major
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
                    if lock(&config.state_dir.join("host.lock")).is_err() {
                        return Err(e);
                    }
                    let tasks: Vec<HostedTask> =
                        match std::fs::read(config.state_dir.join("tasks.json")) {
                            Ok(b) => serde_json::from_slice(&b).map_err(|e| e.to_string())?,
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                            Err(e) => return Err(e.to_string()),
                        };
                    if tasks.iter().any(|t| {
                        matches!(
                            t.snapshot.status,
                            BackgroundTaskStatus::Running | BackgroundTaskStatus::Killing
                        )
                    }) {
                        return Err("Execution Host was lost; persisted services have unknown/unmanaged state. Refusing PID adoption or automatic replacement".into());
                    }
                }
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
            .stderr(std::process::Stdio::null());
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(c) = Self::connect(config.clone()).await {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(c);
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!("Execution Host exited before readiness: {status}"));
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Execution Host readiness timed out".into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    async fn call(&self, request: Request) -> Result<Reply, String> {
        tokio::time::timeout(Duration::from_secs(35), async {
            let mut s = TcpStream::connect(self.ready.address).await.map_err(|e| {
                format!("Execution Host unavailable; service state is unknown: {e}")
            })?;
            write_frame(
                &mut s,
                &Envelope {
                    token: self.ready.token.clone(),
                    major: self.peer_major,
                    request,
                },
            )
            .await?;
            let reply: Reply =
                serde_json::from_slice(&read_frame(&mut s).await?).map_err(|e| e.to_string())?;
            match reply {
                Reply::Error(e) => Err(e),
                v => Ok(v),
            }
        })
        .await
        .map_err(|_| "Execution Host request timeout; outcome unknown".to_string())?
    }
    pub async fn list(&self) -> Result<Vec<HostedTask>, String> {
        match self.call(Request::List).await? {
            Reply::Tasks(t) => Ok(t),
            _ => Err("invalid Execution Host list reply".into()),
        }
    }
    pub async fn spawn(
        &self,
        id: &str,
        request: ProcessRequest,
        owner: &str,
        writer: &str,
        lifetime: BackgroundTaskLifetime,
    ) -> Result<BackgroundTaskSnapshot, SpawnError> {
        // A legacy host predates the `Runtime` owner tag and only admits
        // `Session`/`Persistent`. `Persistent` is its exact name for the same
        // contract — host-owned, survives runtime replacement, ended only by an
        // explicit stop — so a Runtime service must not be refused (or silently
        // fall back to a runtime-local process) just because the persistent
        // owner has not been upgraded yet.
        let lifetime = hosted_wire_lifetime(self.peer_major, lifetime);
        match self
            .call(Request::Spawn {
                id: id.into(),
                request: Box::new(request),
                owner: owner.into(),
                writer: writer.into(),
                lifetime,
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
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
                    },
                );
            }
        }
        let mut out = records.values().cloned().collect::<Vec<_>>();
        out.sort_by(|a, b| a.started_at_ms.cmp(&b.started_at_ms));
        Ok(out)
    }
    async fn persisted_tasks(&self) -> Result<Vec<HostedTask>, String> {
        let _serialization = self.persistence.lock().await;
        let tasks = self.tasks().await?;
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
                    format!(
                        "{:x}",
                        Sha256::digest(serde_json::to_vec(&request).map_err(|e| e.to_string())?)
                    )
                };
                let mut entries = self.entries.lock().await;
                if self.retiring.load(std::sync::atomic::Ordering::SeqCst) {
                    return Ok(Reply::SpawnRejected("Execution Host is retiring".into()));
                }
                if let Some(e) = entries.get(&id) {
                    if e.owner != owner
                        || e.writer != writer
                        || e.fingerprint != fingerprint
                        || e.lifetime != lifetime
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
                    BackgroundTaskLifetime::Runtime
                        | BackgroundTaskLifetime::Session
                        | BackgroundTaskLifetime::Persistent
                ) {
                    return Err("only Runtime/Session/Persistent lifetimes may be hosted".into());
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
                let local = match self
                    .registry
                    .spawn_for_writer(
                        *request,
                        None,
                        Some(&owner),
                        &writer,
                        BackgroundTaskLifetime::Runtime,
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
                            BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
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
                t.id = id;
                self.persist().await?;
                Ok(Reply::Task(t))
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
                            BackgroundTaskStatus::Exited | BackgroundTaskStatus::Killed
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
        return Err("previous Execution Host lost with unmanaged tasks; recovery requires explicit inspection".into());
    }
    match std::fs::read(config.state_dir.join("intents.json")) {
        Ok(b) => {
            let intents: Vec<SpawnIntent> =
                serde_json::from_slice(&b).map_err(|e| e.to_string())?;
            if !intents.is_empty() {
                return Err("previous Execution Host has uncertain spawn intents; refusing automatic recovery".into());
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
    // Subscribe before readiness permits the first client spawn.
    let mut events = host.registry.subscribe();
    host.persist().await?;
    atomic_json(&host.config.state_dir.join("ready.json"), &ready)?;
    let persistence = host.clone();
    tokio::spawn(async move {
        loop {
            let event = tokio::select! { _=persistence.shutdown.cancelled()=>break, event=events.recv()=>event };
            match event {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if let Err(e) = persistence.persist().await {
                        tracing::error!("Execution Host persistence failed: {e}");
                    }
                }
                Err(_) => break,
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
        registry
            .kill_authorized(&id, "foreign", true)
            .await
            .unwrap();
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
                    BackgroundTaskLifetime::Persistent
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
}
