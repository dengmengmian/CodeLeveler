//! `leveler background ...`: read and stop this repository's live background
//! tasks.
//!
//! Runtime-scoped tasks use the runtime protocol. Persistent/session services
//! use their stable execution owner directly, including while no runtime is
//! online. The CLI never signals a process itself.
//!
//! This exists because a handover already names its blockers in the terminal
//! it is waiting in; a user must be able to act on that list from a second
//! shell without opening the interactive UI.

use leveler_project::Layout;

use crate::cli::BackgroundCommand;

#[derive(serde::Serialize)]
struct InventoryEntry {
    task_id: String,
    program: String,
    args: Vec<String>,
    status: &'static str,
    owner: &'static str,
    elapsed_ms: u64,
    session_id: Option<leveler_core::SessionId>,
}

#[allow(unused_variables)]
pub async fn cmd_background(
    layout: Layout,
    command: BackgroundCommand,
) -> anyhow::Result<std::process::ExitCode> {
    #[cfg(any(unix, windows))]
    {
        run(layout, command).await
    }
    #[cfg(not(any(unix, windows)))]
    {
        eprintln!(
            "background tasks are managed through the local runtime; \
             this platform has no local socket transport"
        );
        Ok(std::process::ExitCode::from(1))
    }
}

#[cfg(any(unix, windows))]
async fn run(layout: Layout, command: BackgroundCommand) -> anyhow::Result<std::process::ExitCode> {
    use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient};
    use leveler_execution::BackgroundTaskStatus;
    use leveler_execution::execution_host::ExecutionHostClient;

    let socket = layout.socket_path();
    let client = leveler_runtime_host::probe_default_runtime(&socket).await?;
    let blockers = match client.as_ref() {
        Some(client) => {
            leveler_local_transport::LocalRuntimeService::runtime_info(client)
                .await
                .map_err(|error| anyhow::anyhow!("could not read runtime status: {error}"))?
                .health
                .blockers
        }
        None => Vec::new(),
    };
    // A reported runtime blocker is owned by that runtime. An unrelated host's
    // upgrade failure must not prevent users from inspecting or stopping it.
    let runtime_task = match &command {
        BackgroundCommand::Logs { task_id } | BackgroundCommand::Stop { task_id } => {
            blockers.iter().find(|blocker| blocker.task_id == *task_id)
        }
        BackgroundCommand::List { .. } => None,
    };
    match (&command, runtime_task) {
        (BackgroundCommand::Logs { task_id }, Some(blocker)) => {
            print_log(task_id, &blocker.log_tail);
            return Ok(std::process::ExitCode::SUCCESS);
        }
        (BackgroundCommand::Stop { task_id }, Some(blocker)) => {
            let Some(session_id) = blocker.session_id.clone() else {
                anyhow::bail!(
                    "background task `{task_id}` is not owned by a session and cannot be stopped \
                     from a client"
                );
            };
            client
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("task runtime is offline"))?
                .send(ClientCommand::CancelBackgroundTask {
                    session_id,
                    task_id: task_id.clone(),
                })
                .await
                .map_err(|error| anyhow::anyhow!("stop request refused: {error}"))?;
            println!("stop requested for {task_id}");
            return Ok(std::process::ExitCode::SUCCESS);
        }
        _ => {}
    }
    let host = ExecutionHostClient::probe(&leveler_app::execution_host_config(&layout)?)
        .await
        .map_err(anyhow::Error::msg)?;
    let hosted = match host.as_ref() {
        Some(host) => host.list().await.map_err(anyhow::Error::msg)?,
        None => Vec::new(),
    };

    match command {
        BackgroundCommand::List { json } => {
            let mut inventory = blockers
                .iter()
                .map(|task| InventoryEntry {
                    task_id: task.task_id.clone(),
                    program: task.program.clone(),
                    args: task.args.clone(),
                    status: "running",
                    owner: "runtime",
                    elapsed_ms: task.elapsed_ms,
                    session_id: task.session_id.clone(),
                })
                .collect::<Vec<_>>();
            inventory.extend(hosted.iter().map(|task| {
                let snapshot = &task.snapshot;
                InventoryEntry {
                    task_id: snapshot.id.clone(),
                    program: snapshot.program.clone(),
                    args: snapshot.args.clone(),
                    status: match snapshot.status {
                        BackgroundTaskStatus::Running => "running",
                        BackgroundTaskStatus::Killing => "killing",
                        BackgroundTaskStatus::Exited => "exited",
                        BackgroundTaskStatus::Killed => "killed",
                    },
                    owner: "execution-host",
                    elapsed_ms: snapshot.duration_ms,
                    session_id: snapshot
                        .owner_scope
                        .clone()
                        .map(leveler_core::SessionId::new),
                }
            }));
            if json {
                println!("{}", serde_json::to_string_pretty(&inventory)?);
            } else if inventory.is_empty() {
                println!("no background tasks");
            } else {
                for task in &inventory {
                    println!(
                        "{}  {}  {}  {}  {}",
                        task.task_id,
                        task.status,
                        task.owner,
                        crate::run_cmds::blocker_command_line(&task.program, &task.args),
                        crate::run_cmds::format_task_age(task.elapsed_ms),
                    );
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        BackgroundCommand::Logs { task_id } => {
            if let Some(task) = hosted.iter().find(|task| task.snapshot.id == task_id) {
                let owner = task
                    .snapshot
                    .owner_scope
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("hosted task has no session owner"))?;
                let snapshot = host
                    .as_ref()
                    .expect("hosted inventory requires a host")
                    .get_owned(&task_id, owner)
                    .await
                    .map_err(anyhow::Error::msg)?;
                print_log(&task_id, &snapshot.log);
                return Ok(std::process::ExitCode::SUCCESS);
            }
            anyhow::bail!("no live background task `{task_id}`");
        }
        BackgroundCommand::Stop { task_id } => {
            if let Some(task) = hosted.iter().find(|task| task.snapshot.id == task_id) {
                let owner = task
                    .snapshot
                    .owner_scope
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("hosted task has no session owner"))?;
                host.as_ref()
                    .expect("hosted inventory requires a host")
                    .kill_owned(&task_id, owner)
                    .await
                    .map_err(anyhow::Error::msg)?;
                println!("stop requested for {task_id}");
                return Ok(std::process::ExitCode::SUCCESS);
            }
            anyhow::bail!("no live background task `{task_id}`");
        }
    }
}

fn print_log(task_id: &str, log: &str) {
    if log.trim().is_empty() {
        println!("(no output captured for {task_id})");
    } else {
        print!("{log}");
        if !log.ends_with('\n') {
            println!();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use leveler_client_protocol::{
        ClientCommand, ClientError, InteractiveRuntimeClient, RuntimeEvent, RuntimeHealth,
        RuntimeInfo, UiBackgroundTaskBlocker, UiSessionSnapshot,
    };
    use leveler_local_transport::{CreateSessionRequest, LocalRuntimeService, SessionBootstrap};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use tokio::sync::broadcast;

    struct Runtime {
        commands: Mutex<Vec<ClientCommand>>,
        events: broadcast::Sender<RuntimeEvent>,
    }

    #[async_trait::async_trait]
    impl InteractiveRuntimeClient for Runtime {
        async fn send(&self, command: ClientCommand) -> Result<(), ClientError> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }
        fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
            self.events.subscribe()
        }
        async fn snapshot(
            &self,
            _: &leveler_core::SessionId,
        ) -> Result<UiSessionSnapshot, ClientError> {
            Err(ClientError::Runtime("not exercised".into()))
        }
    }

    #[async_trait::async_trait]
    impl LocalRuntimeService for Runtime {
        async fn create_session(
            &self,
            _: CreateSessionRequest,
        ) -> Result<SessionBootstrap, ClientError> {
            Err(ClientError::Runtime("not exercised".into()))
        }
        async fn runtime_info(&self) -> Result<RuntimeInfo, ClientError> {
            Ok(RuntimeInfo {
                runtime_id: leveler_core::RuntimeId::new("runtime"),
                version: "old-runtime".into(),
                build: leveler_core::BuildIdentity::current(),
                config_fingerprint: None,
                pid: std::process::id(),
                health: RuntimeHealth {
                    active_background_tasks: 1,
                    blockers: vec![UiBackgroundTaskBlocker {
                        task_id: "bg-1".into(),
                        program: "make".into(),
                        args: vec!["up".into()],
                        elapsed_ms: 1000,
                        session_id: Some(leveler_core::SessionId::new("owner")),
                        log_tail: "runtime output\n".into(),
                    }],
                    ..Default::default()
                },
            })
        }
    }

    async fn with_incompatible_host(
        command: BackgroundCommand,
    ) -> (anyhow::Result<std::process::ExitCode>, Vec<ClientCommand>) {
        // Keep Unix socket paths short even on macOS's long temporary root.
        let tmp = tempfile::Builder::new()
            .prefix("bg-")
            .tempdir_in("/tmp")
            .unwrap();
        let layout = Layout::from_parts(
            tmp.path().join("repo"),
            tmp.path().join("config"),
            tmp.path().join("state"),
        );
        let host_dir = layout.state_dir.join("execution-host");
        std::fs::create_dir_all(&host_dir).unwrap();
        std::fs::set_permissions(&host_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ready = host_dir.join("ready.json");
        let metadata = serde_json::json!({
            "address": "127.0.0.1:1", "token": "test", "instance": "old-host",
            "pid": std::process::id(), "repo": layout.require_workspace().unwrap(),
            "major": 1, "minor": 0, "capabilities": []
        });
        std::fs::write(&ready, serde_json::to_vec(&metadata).unwrap()).unwrap();
        std::fs::set_permissions(&ready, std::fs::Permissions::from_mode(0o600)).unwrap();
        let runtime = Arc::new(Runtime {
            commands: Mutex::new(Vec::new()),
            events: broadcast::channel(16).0,
        });
        let server =
            leveler_local_transport::LocalSocketServer::bind(layout.socket_path(), runtime.clone())
                .await
                .unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(server.serve(shutdown.clone()));
        let result = run(layout, command).await;
        shutdown.cancel();
        task.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read(&ready).unwrap(),
            serde_json::to_vec(&metadata).unwrap(),
            "old host metadata must be preserved"
        );
        let commands = runtime.commands.lock().unwrap().clone();
        (result, commands)
    }

    #[tokio::test]
    async fn runtime_logs_ignore_unrelated_incompatible_host() {
        let (result, commands) = with_incompatible_host(BackgroundCommand::Logs {
            task_id: "bg-1".into(),
        })
        .await;
        assert_eq!(result.unwrap(), std::process::ExitCode::SUCCESS);
        assert!(commands.is_empty());
    }

    #[tokio::test]
    async fn runtime_stop_reaches_owner_despite_incompatible_host() {
        let (result, commands) = with_incompatible_host(BackgroundCommand::Stop {
            task_id: "bg-1".into(),
        })
        .await;
        assert_eq!(result.unwrap(), std::process::ExitCode::SUCCESS);
        assert!(
            matches!(commands.as_slice(), [ClientCommand::CancelBackgroundTask { session_id, task_id }] if session_id.as_str() == "owner" && task_id == "bg-1")
        );
    }

    #[tokio::test]
    async fn host_inventory_errors_remain_explicit() {
        for command in [
            BackgroundCommand::List { json: true },
            BackgroundCommand::Logs {
                task_id: "host-task".into(),
            },
            BackgroundCommand::Stop {
                task_id: "host-task".into(),
            },
        ] {
            let (result, commands) = with_incompatible_host(command).await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("protocol major 1 is incompatible with 2")
            );
            assert!(commands.is_empty());
        }
    }
}
