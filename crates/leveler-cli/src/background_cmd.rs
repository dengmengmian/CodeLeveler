//! `leveler background ...`: read and stop this repository's live background
//! tasks.
//!
//! The local runtime owns the tasks and their process groups; this CLI is a
//! thin controller over the same socket protocol a TUI uses. `stop` never
//! signals a process itself — it sends `CancelBackgroundTask` and the runtime,
//! which owns the process group, terminates the tree.
//!
//! This exists because a handover already names its blockers in the terminal
//! it is waiting in; a user must be able to act on that list from a second
//! shell without opening the interactive UI.

use leveler_project::Layout;

use crate::cli::BackgroundCommand;

#[allow(unused_variables)]
pub async fn cmd_background(
    layout: Layout,
    command: BackgroundCommand,
) -> anyhow::Result<std::process::ExitCode> {
    #[cfg(unix)]
    {
        run(layout, command).await
    }
    #[cfg(not(unix))]
    {
        eprintln!(
            "background tasks are managed through the local runtime; \
             this platform has no local socket transport"
        );
        Ok(std::process::ExitCode::from(1))
    }
}

#[cfg(unix)]
async fn run(layout: Layout, command: BackgroundCommand) -> anyhow::Result<std::process::ExitCode> {
    use leveler_client_protocol::{ClientCommand, InteractiveRuntimeClient};

    let socket = layout.socket_path();
    let Some(client) = crate::run_cmds::connect_default_runtime(&socket).await? else {
        anyhow::bail!(
            "no local runtime is running for this repository; start `leveler` (or `leveler serve`) first"
        );
    };
    let info = leveler_local_transport::LocalRuntimeService::runtime_info(&client)
        .await
        .map_err(|error| anyhow::anyhow!("could not read runtime status: {error}"))?;
    let blockers = info.health.blockers;

    match command {
        BackgroundCommand::List { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&blockers)?);
            } else if blockers.is_empty() {
                println!("no live background tasks");
            } else {
                for blocker in &blockers {
                    println!(
                        "{}  {}  {}",
                        blocker.task_id,
                        crate::run_cmds::blocker_command_line(&blocker.program, &blocker.args),
                        crate::run_cmds::format_task_age(blocker.elapsed_ms),
                    );
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        BackgroundCommand::Logs { task_id } => {
            let Some(blocker) = blockers.iter().find(|blocker| blocker.task_id == task_id) else {
                anyhow::bail!("no live background task `{task_id}`");
            };
            if blocker.log_tail.trim().is_empty() {
                println!("(no output captured for {task_id})");
            } else {
                print!("{}", blocker.log_tail);
                if !blocker.log_tail.ends_with('\n') {
                    println!();
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        BackgroundCommand::Stop { task_id } => {
            let Some(blocker) = blockers.iter().find(|blocker| blocker.task_id == task_id) else {
                anyhow::bail!("no live background task `{task_id}`");
            };
            let Some(session_id) = blocker.session_id.clone() else {
                anyhow::bail!(
                    "background task `{task_id}` is not owned by a session and cannot be stopped \
                     from a client"
                );
            };
            client
                .send(ClientCommand::CancelBackgroundTask {
                    session_id,
                    task_id: task_id.clone(),
                })
                .await
                .map_err(|error| anyhow::anyhow!("stop request refused: {error}"))?;
            println!("stop requested for {task_id}");
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}
