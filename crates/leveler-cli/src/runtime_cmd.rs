use std::process::ExitCode;

use crate::cli::RuntimeCommand;
use leveler_project::Layout;

#[cfg(any(unix, windows))]
pub(crate) async fn run(layout: &Layout, command: RuntimeCommand) -> anyhow::Result<ExitCode> {
    use leveler_runtime_host::StopRuntime;

    let zh = matches!(leveler_tui::Locale::resolve(None), leveler_tui::Locale::Zh);
    let text = |zh_text, en_text| if zh { zh_text } else { en_text };
    let path = layout.socket_path();
    let (message, code) = match command {
        RuntimeCommand::Status { json } => {
            let inspection = leveler_runtime_host::inspect_runtime(&path).await?;
            print_status(layout, inspection, json, zh)?;
            return Ok(ExitCode::SUCCESS);
        }
        RuntimeCommand::Stop => match leveler_runtime_host::stop_runtime(&path).await? {
            StopRuntime::Absent => (
                text(
                    "当前项目没有运行中的 Runtime。",
                    "No runtime is serving this workspace.",
                ),
                ExitCode::SUCCESS,
            ),
            StopRuntime::Stopped => (
                text("Runtime 已正常退出。", "The runtime stopped."),
                ExitCode::SUCCESS,
            ),
            StopRuntime::Busy {
                active_turns,
                active_background_tasks,
            } => {
                eprintln!(
                    "{} ({active_turns} {}, {active_background_tasks} {})",
                    text(
                        "请等待现有任务完成后再停止 Runtime",
                        "Wait for the existing work to finish before stopping the runtime"
                    ),
                    text("个轮次", "turns"),
                    text("个后台任务", "background tasks"),
                );
                return Ok(ExitCode::FAILURE);
            }
            StopRuntime::Legacy => (
                text(
                    "无法验证此版本的安全退出。请待任务完成后从旧实例正常退出。",
                    "Safe retirement cannot be verified for this version. Let its work finish and exit the old instance normally.",
                ),
                ExitCode::FAILURE,
            ),
            StopRuntime::IdentityUnknown => (
                text(
                    "无法验证 Runtime 身份。请查看 runtime status。",
                    "The runtime identity could not be verified. Check runtime status.",
                ),
                ExitCode::FAILURE,
            ),
            StopRuntime::GenerationChanged => (
                text(
                    "Runtime 实例已变更。请查看 runtime status。",
                    "The runtime changed during the request. Check runtime status.",
                ),
                ExitCode::FAILURE,
            ),
            StopRuntime::Unresponsive => (
                text(
                    "Runtime 未响应。请查看 runtime status。",
                    "The runtime did not respond. Check runtime status.",
                ),
                ExitCode::FAILURE,
            ),
            StopRuntime::OutcomeUnknown => (
                text(
                    "无法确认 Runtime 是否接受退出。请查看 runtime status。",
                    "The retirement decision could not be confirmed. Check runtime status.",
                ),
                ExitCode::FAILURE,
            ),
            StopRuntime::ExitUnconfirmed => (
                text(
                    "Runtime 已接受退休，尚未确认退出。请查看 runtime status。",
                    "The runtime accepted retirement and its exit is pending. Check runtime status.",
                ),
                ExitCode::FAILURE,
            ),
        },
    };
    if code == ExitCode::SUCCESS {
        println!("{message}");
    } else {
        eprintln!("{message}");
    }
    Ok(code)
}

#[cfg(any(unix, windows))]
fn print_status(
    layout: &Layout,
    inspection: leveler_runtime_host::RuntimeInspection,
    json: bool,
    zh: bool,
) -> anyhow::Result<()> {
    use leveler_runtime_host::{RuntimeConsistency, RuntimeInspection};
    use serde_json::json;

    let (presence, info, local_waiters, compatibility) = match inspection {
        RuntimeInspection::Absent => ("absent", None, None, None),
        RuntimeInspection::Unresponsive => ("unresponsive", None, None, None),
        RuntimeInspection::Legacy {
            info,
            local_waiters,
        } => (
            "legacy",
            info,
            local_waiters,
            Some("safe_handoff_unverified"),
        ),
        RuntimeInspection::Authenticated {
            info,
            local_waiters,
        } => {
            let compatibility = leveler_runtime_host::expected_config_fingerprint(layout)
                .ok()
                .and_then(|fingerprint| {
                    match leveler_runtime_host::classify_runtime_generation(
                        info.as_deref(),
                        &leveler_core::BuildIdentity::current(),
                        &fingerprint,
                    ) {
                        RuntimeConsistency::Current => Some("current"),
                        RuntimeConsistency::Outdated { .. } => Some("build_mismatch"),
                        RuntimeConsistency::ConfigChanged => Some("config_changed"),
                        RuntimeConsistency::Unknown => None,
                    }
                });
            ("authenticated", info, local_waiters, compatibility)
        }
    };
    let info = info.as_deref();
    let build = info
        .map(|info| &info.build)
        .filter(|build| build.is_known());
    let pid = info.map(|info| info.pid).filter(|pid| *pid != 0);
    // Older replies can omit the whole health object. A reported admission
    // capacity distinguishes the populated health we can interpret as counts.
    let health = info
        .map(|info| &info.health)
        .filter(|health| health.turn_capacity.is_some());
    let lifecycle = match health {
        Some(health) if health.shutting_down => Some("retiring"),
        Some(health) if health.quiescent() => Some("quiescent"),
        Some(_) => Some("busy"),
        None => None,
    };
    let idle = info.and_then(|info| info.idle_eviction.as_ref());
    let status = json!({
        "presence": presence,
        "build": build,
        "version": info.map(|info| &info.version).filter(|version| !version.is_empty()),
        "pid": pid,
        "process_start_identity": pid.and_then(leveler_execution::process_identity::process_start_identity),
        "runtime_id": info.map(|info| info.runtime_id.as_str()).filter(|id| !id.is_empty()),
        "lifecycle": lifecycle,
        "active_turns": health.map(|health| health.active_turns),
        "active_background_tasks": health.map(|health| health.active_background_tasks),
        "local_waiters": local_waiters,
        "idle_eviction": idle,
        "compatibility": compatibility,
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        let fields = [
            ("presence", "连接", "Endpoint"),
            ("version", "版本", "Version"),
            ("build", "构建", "Build"),
            ("pid", "进程", "PID"),
            (
                "process_start_identity",
                "进程启动标识",
                "Process start identity",
            ),
            ("runtime_id", "Runtime ID", "Runtime ID"),
            ("lifecycle", "生命周期", "Lifecycle"),
            ("active_turns", "活跃轮次", "Active turns"),
            ("active_background_tasks", "后台任务", "Background tasks"),
            ("local_waiters", "本地界面", "Attached local clients"),
            ("compatibility", "兼容性", "Compatibility"),
        ];
        for (key, zh_label, en_label) in fields {
            print_status_field(zh, zh_label, en_label, &status[key]);
        }
        let idle = &status["idle_eviction"];
        for (key, zh_label, en_label) in [
            ("state", "空闲回收状态", "Idle eviction"),
            (
                "effective_idle_timeout_secs",
                "空闲超时（秒）",
                "Idle timeout (s)",
            ),
            ("idle_for_ms", "已空闲时长（毫秒）", "Idle time (ms)"),
            ("last_error", "最近回收错误", "Last eviction error"),
        ] {
            print_status_field(zh, zh_label, en_label, &idle[key]);
        }
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn print_status_field(zh: bool, zh_label: &str, en_label: &str, value: &serde_json::Value) {
    let rendered = match value {
        serde_json::Value::Null => if zh { "未知" } else { "unknown" }.to_owned(),
        serde_json::Value::String(value) => {
            leveler_core::sanitize_terminal_output(value).replace('\n', " ")
        }
        _ => value.to_string(),
    };
    println!("{}  {rendered}", if zh { zh_label } else { en_label });
}

#[cfg(not(any(unix, windows)))]
pub(crate) async fn run(_layout: &Layout, _command: RuntimeCommand) -> anyhow::Result<ExitCode> {
    anyhow::bail!("Local runtimes are unavailable on this platform")
}
