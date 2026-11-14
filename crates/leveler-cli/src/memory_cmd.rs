//! `leveler memory` — inspect and manage durable cross-session memory.
//!
//! This is the same projection and the same store the TUI and the agent use:
//! `status` reads the runtime's own durable counters, `show` performs the same
//! bounded, containment-checked read the memory page does, and `forget` routes
//! through the maintenance store's tombstone path. None of these commands
//! invents a model turn.

use leveler_app::memory::MemoryRuntime;
use leveler_client_protocol::UiMemoryStatus;
use leveler_project::Layout;

use crate::cli::MemoryCommand;

pub async fn cmd_memory(
    layout: Layout,
    command: MemoryCommand,
) -> anyhow::Result<std::process::ExitCode> {
    let config = leveler_app::ConfigManager::load(layout.clone())?;
    let runtime = MemoryRuntime::new(&layout, config);
    match command {
        MemoryCommand::Status { json } => {
            let status = runtime.ui_status(None).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
        }
        MemoryCommand::Show { scope, path, json } => {
            let file = runtime.read_ui_file(&scope, &path).await;
            if let Some(error) = file.error.as_deref() {
                anyhow::bail!("{error}");
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&file)?);
            } else {
                print!("{}", file.content);
                if !file.content.ends_with('\n') {
                    println!();
                }
                if file.truncated {
                    eprintln!(
                        "\n[truncated: showing the first {} of {} bytes]",
                        file.content.len(),
                        file.total_bytes
                    );
                }
                // The hash is what `forget` binds to. Keep it off stdout so a
                // piped note stays copy-pasteable.
                eprintln!("blake3: {}", file.content_hash);
            }
        }
        MemoryCommand::Remember { text } => {
            let saved = runtime.remember(&text).await.map_err(anyhow::Error::msg)?;
            match saved {
                Some(path) => println!("Saved to global memory: {}", path.display()),
                None => anyhow::bail!("nothing to remember: the note was empty"),
            }
        }
        MemoryCommand::Forget {
            scope,
            path,
            content_hash,
        } => {
            let result = runtime.forget_ui_file(&scope, &path, &content_hash).await;
            if !result.ok {
                anyhow::bail!("{}", result.message);
            }
            println!("{}", result.message);
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn print_status(status: &UiMemoryStatus) {
    let enabled = |value: bool| if value { "on" } else { "off" };
    println!(
        "rollout: {}  capture: {}  dream: automatic {}, manual {}, batch {}",
        status.rollout,
        enabled(status.capture_enabled),
        enabled(status.automatic_dream_enabled),
        enabled(status.manual_dream_enabled),
        enabled(status.batch_dream_enabled),
    );
    let opened = status.rollout != "off";
    if !opened {
        println!(
            "memory is off; nothing is captured or injected for this configuration \
             (`memory remember` still records an explicit note)"
        );
    }
    print_scope("global", &status.global, opened);
    print_scope("workspace", &status.workspace, opened);
    if let Some(session) = &status.session {
        println!(
            "session {}: requested {}, captured {}, indexed {}, pending {}, active {}, failed {}",
            session.session_id,
            session.requested,
            session.captured,
            session.indexed,
            session.pending,
            session.active,
            session.failed,
        );
    }
    println!(
        "activity: {}{}",
        status.activity.kind,
        if status.activity.active {
            " (running)"
        } else {
            ""
        }
    );
    if let Some(error) = &status.activity.last_error {
        println!("last error: {error}");
    }
    if status.workspace_ephemeral {
        println!("workspace memory is ephemeral for this process");
    }
}

fn print_scope(name: &str, scope: &leveler_client_protocol::UiMemoryScope, opened: bool) {
    println!("\n{name}: {}", scope.root);
    if let Some(error) = &scope.error {
        println!("  error: {error}");
        return;
    }
    if !opened {
        println!("  not read: memory is off");
        return;
    }
    println!(
        "  capture jobs: {} pending, {} running, {} retry, {} failed, {} completed",
        scope.pending_capture_jobs,
        scope.running_capture_jobs,
        scope.retry_capture_jobs,
        scope.failed_capture_jobs,
        scope.completed_capture_jobs,
    );
    println!(
        "  observations: {} pending, {} hidden · archives {} · tombstones {}",
        scope.pending_observations,
        scope.hidden_pending_observations,
        scope.archive_count,
        scope.tombstone_count,
    );
    println!(
        "  dream lease: {} · last success {} · last failure {}",
        scope.dream_lease,
        human_time(scope.last_success_at),
        human_time(scope.last_failure_at),
    );
    if scope.files.is_empty() {
        println!("  files: (none)");
        return;
    }
    println!("  files:");
    for file in &scope.files {
        println!(
            "    {:<48} {:>9}  {}{}",
            file.path,
            human_bytes(file.bytes),
            file.kind,
            if file.deletable { "" } else { "  (protected)" },
        );
    }
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn human_time(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_owned())
}
