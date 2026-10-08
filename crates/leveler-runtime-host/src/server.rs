use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_local_transport::{
    LocalRuntimeService, LocalSocketServer, LocalWaiters, TcpRuntimeServer,
};

pub const DAEMON_TOKEN_ENV: &str = "LEVELER_DAEMON_TOKEN";
const DAEMON_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub fn daemon_idle_timeout() -> Duration {
    std::env::var("LEVELER_DAEMON_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DAEMON_IDLE_TIMEOUT)
}

pub fn generate_daemon_token() -> String {
    use std::fmt::Write;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG unavailable");
    let mut token = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(token, "{byte:02x}");
    }
    token
}

/// Both listeners stay bound until their owners finish serving. The Unix
/// listener is the repository ownership proof, including in TCP mode.
pub struct BoundServers {
    pub unix: Option<LocalSocketServer>,
    pub tcp: Option<(TcpRuntimeServer, String)>,
}

pub struct PreparedDaemon {
    pub bound: BoundServers,
    pub runtime_id: String,
}

pub async fn bind_daemon_transports(
    socket_path: &Path,
    tcp: Option<SocketAddr>,
    token: Option<String>,
    service: Arc<dyn LocalRuntimeService>,
    local_waiters: LocalWaiters,
) -> anyhow::Result<BoundServers> {
    let unix =
        LocalSocketServer::bind_with_waiters(socket_path, service.clone(), local_waiters.clone())
            .await?;
    let tcp = match tcp {
        Some(addr) => {
            let token = token.unwrap_or_else(generate_daemon_token);
            let server =
                TcpRuntimeServer::bind_with_waiters(addr, token.clone(), service, local_waiters)
                    .await?;
            Some((server, token))
        }
        None => None,
    };
    Ok(BoundServers {
        unix: Some(unix),
        tcp,
    })
}

/// The existing machine-readable readiness contract. Publishing this before
/// recovery would falsely claim that the runtime is ready for clients.
pub fn ready_json(
    socket_path: &Path,
    tcp: Option<&(TcpRuntimeServer, String)>,
    runtime_id: &str,
) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "pid": std::process::id(),
        "socket": socket_path,
        "addr": tcp.map(|(server, _)| server.local_addr()).transpose()?.map(|addr| addr.to_string()),
        "token": tcp.map(|(_, token)| token.clone()),
        "runtime_id": runtime_id,
    }))
}

/// Binding establishes exclusive ownership before any recovery write. The
/// application and engine retain their recovery algorithms; the host owns
/// the order of those operations and publishes readiness last.
pub async fn prepare_daemon(
    app: &Arc<Application>,
    runtime: &Arc<InProcessRuntimeClient>,
    socket_path: &Path,
    tcp: Option<SocketAddr>,
    ready_path: Option<&Path>,
    local_waiters: LocalWaiters,
) -> anyhow::Result<PreparedDaemon> {
    let token = tcp.map(|_| {
        std::env::var(DAEMON_TOKEN_ENV)
            .ok()
            .filter(|token| !token.is_empty())
            .unwrap_or_else(generate_daemon_token)
    });
    let service: Arc<dyn LocalRuntimeService> = runtime.clone();
    let bound = bind_daemon_transports(socket_path, tcp, token, service, local_waiters).await?;
    app.reconcile_execution_services().await?;
    runtime.spawn_idle_eviction(daemon_idle_timeout());

    let runtime_id = app.runtime_id()?;
    let db = app.open_database().await?;
    let engine = app.task_engine(&db)?;
    let reap =
        leveler_engine::reap_after_restart(&engine, None, leveler_engine::ReapScope::EndedBoots)
            .await?;
    app.finish_reaped_sessions(&engine, &reap.reaped_sessions)
        .await?;
    for conflict in &reap.conflicts {
        tracing::warn!(session = conflict.session_id.as_str(), refusal = ?conflict.refusal,
            "not reaping running turns without proof their boot has ended");
    }
    if !reap.events.is_empty() {
        tracing::warn!(
            reaped = reap.events.len(),
            "reaped zombie turns before daemon startup"
        );
    }
    app.start_memory_consolidator().await?;
    if let Some(path) = ready_path {
        let ready = ready_json(socket_path, bound.tcp.as_ref(), runtime_id.as_str())?;
        std::fs::write(path, serde_json::to_vec_pretty(&ready)?)?;
    }
    Ok(PreparedDaemon {
        bound,
        runtime_id: runtime_id.to_string(),
    })
}
