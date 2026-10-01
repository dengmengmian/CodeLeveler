//! The `/btw` side question's read-only runtime projection.
//!
//! A side question is an observer of the main task, not a second agent: it may
//! read the runtime's own facts (turn state, active tool calls, background
//! tasks and their retained output) but must never start work, mutate the
//! workspace, or steer the main task. This module owns exactly one thing —
//! turning the runtime's authoritative state into a bounded, id-carrying
//! text block — so the model answers from the same world the Background Tasks
//! page renders instead of guessing from `tmp/run/*.log` and pid files.
//!
//! Grounding priority, highest first:
//!   1. live runtime state (this block)
//!   2. current execution record (active tool call / transcript history)
//!   3. current session structured history (assembled by the caller)
//!   4. workspace read tools
//!   5. unbound historical artifacts — never current state
//!
//! Everything here is bound to a `session_id` / tool-call id / `task_id`.
//! A fact that carries no such binding is not emitted as current state.

use std::fmt::Write as _;

/// One active tool call, as the live view holds it.
pub(crate) struct ActiveToolFact {
    pub name: String,
    pub elapsed_ms: u64,
    /// Bounded tail of the running call's output.
    pub output_tail: String,
}

/// One background task the runtime still owns or recently finished.
pub(crate) struct BackgroundTaskFact {
    pub task_id: String,
    pub command: String,
    pub pid: Option<u32>,
    pub status: &'static str,
    pub elapsed_ms: u64,
    pub exit_code: Option<i32>,
    /// Bounded tail of the task's retained combined stdout/stderr.
    pub output_tail: String,
}

/// Everything the observer needs, already copied out of the runtime so the
/// renderer holds no lock and cannot block the main turn.
pub(crate) struct SideQuestionContext {
    pub session_id: String,
    pub cwd: Option<String>,
    /// The main turn, when one is admitted for this session.
    pub turn_elapsed_ms: Option<u64>,
    pub turn_idle_ms: Option<u64>,
    /// A mechanical classification of what the main turn is blocked on,
    /// derived from the facts below — never guessed by the model.
    pub wait_on: Option<String>,
    pub active_tools: Vec<ActiveToolFact>,
    pub background_tasks: Vec<BackgroundTaskFact>,
}

/// Cap on one task's injected tail. Larger tails are available on demand
/// through the existing read-only `get_task` tool.
const TASK_TAIL_BYTES: usize = 4 * 1024;
/// Cap on one active tool call's injected tail.
const TOOL_TAIL_BYTES: usize = 1024;
/// Bound the number of task facts so a busy session cannot flood the prompt.
const MAX_TASK_FACTS: usize = 16;

/// Render the projection as a bounded, clearly-provenanced block.
pub(crate) fn render(ctx: &SideQuestionContext) -> String {
    let mut out = String::new();
    out.push_str("【当前运行时状态 · 只读快照】\n");
    let _ = writeln!(out, "session: {}", ctx.session_id);
    let _ = writeln!(
        out,
        "cwd: {}",
        ctx.cwd
            .as_deref()
            .unwrap_or("unavailable (no primary workspace)")
    );

    match (ctx.turn_elapsed_ms, ctx.turn_idle_ms) {
        (Some(elapsed), Some(idle)) => {
            let _ = writeln!(
                out,
                "主任务: 运行中（已运行 {}，静止 {}）",
                fmt_ms(elapsed),
                fmt_ms(idle)
            );
        }
        _ => out.push_str("主任务: 空闲（当前没有活动中的主回合）\n"),
    }
    match &ctx.wait_on {
        Some(reason) => {
            let _ = writeln!(out, "当前等待: {reason}");
        }
        None if ctx.turn_elapsed_ms.is_some() => {
            out.push_str("当前等待: 正在等待模型响应\n");
        }
        None => {}
    }

    if ctx.active_tools.is_empty() {
        out.push_str("活动工具调用: 无\n");
    } else {
        let _ = writeln!(out, "活动工具调用: {}", ctx.active_tools.len());
        for tool in ctx.active_tools.iter().take(MAX_TASK_FACTS) {
            let _ = writeln!(
                out,
                "  - {} · 已运行 {}",
                tool.name,
                fmt_ms(tool.elapsed_ms)
            );
            push_tail(&mut out, &tool.output_tail, TOOL_TAIL_BYTES, "      ");
        }
    }

    if ctx.background_tasks.is_empty() {
        out.push_str("后台任务: 无\n");
    } else {
        let _ = writeln!(out, "后台任务: {}", ctx.background_tasks.len());
        for task in ctx.background_tasks.iter().take(MAX_TASK_FACTS) {
            let pid = task
                .pid
                .map(|pid| format!(" · PID {pid}"))
                .unwrap_or_default();
            let exit = task
                .exit_code
                .map(|code| format!(" · exit {code}"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "  - {} · {} · {}{} · {}{}",
                task.task_id,
                task.command,
                task.status,
                pid,
                fmt_ms(task.elapsed_ms),
                exit
            );
            push_tail(&mut out, &task.output_tail, TASK_TAIL_BYTES, "      ");
        }
    }

    out.push_str(
        "\n以上是运行时权威事实，均已绑定到 session / tool call / task id。\
         \n工作区中的 tmp/run/*.log、*.pid、旧构建或测试日志等，若不能证明属于上面的 id，\
         只能视为历史资料，不能用来判断当前主任务或后台任务的状态。\
         \n需要某个后台任务的更多日志时，用只读工具 get_task(task_id=…)。\
         \n本快照没有的观测值就是运行时没有该事实：请直接说明，不要再去执行 shell 命令探测。",
    );
    out
}

/// Append a tail, elided to `max` bytes from the FRONT so the newest output —
/// the part a status question is about — always survives.
fn push_tail(out: &mut String, tail: &str, max: usize, indent: &str) {
    if tail.trim().is_empty() {
        return;
    }
    let body = if tail.len() > max {
        let cut = leveler_core::ceil_char_boundary(tail, tail.len() - max);
        format!("…[前段已省略]…\n{}", &tail[cut..])
    } else {
        tail.to_string()
    };
    out.push_str(indent);
    out.push_str("输出尾部:\n");
    for line in body.lines() {
        let _ = writeln!(out, "{indent}  {}", line);
    }
}

/// The main turn's mechanical blocker, derived from facts alone — the runtime
/// never asks the model what it is waiting on.
pub(crate) fn derive_wait(running_tasks: &[String], active_tools: &[String]) -> Option<String> {
    if let Some(task) = running_tasks.first() {
        return Some(match running_tasks.len() {
            1 => format!("后台任务 {task}"),
            n => format!("{n} 个后台任务（含 {task}）"),
        });
    }
    active_tools.first().map(|tool| format!("工具调用 {tool}"))
}

/// Human-readable duration. Mechanical formatting only.
fn fmt_ms(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SideQuestionContext {
        SideQuestionContext {
            session_id: "s1".into(),
            cwd: Some("/w".into()),
            turn_elapsed_ms: Some(402_000),
            turn_idle_ms: Some(2_000),
            wait_on: Some("后台任务 bg-2".into()),
            active_tools: vec![ActiveToolFact {
                name: "run_command".into(),
                elapsed_ms: 402_000,
                output_tail: "[stdout] building\n".into(),
            }],
            background_tasks: vec![BackgroundTaskFact {
                task_id: "bg-2".into(),
                command: "make up".into(),
                pid: Some(38172),
                status: "running",
                elapsed_ms: 402_000,
                exit_code: None,
                output_tail: "[stdout] ready\n[stderr] warn\n".into(),
            }],
        }
    }

    #[test]
    fn projection_carries_identity_and_facts() {
        let text = render(&sample());
        assert!(text.contains("session: s1"), "{text}");
        assert!(text.contains("bg-2"), "{text}");
        assert!(text.contains("make up"), "{text}");
        assert!(text.contains("PID 38172"), "{text}");
        assert!(text.contains("6m42s"), "{text}");
        assert!(
            text.contains("最后输出") || text.contains("输出尾部"),
            "{text}"
        );
        // The provenance rule is part of the contract, not model guesswork.
        assert!(text.contains("历史资料"), "{text}");
        assert!(text.contains("get_task"), "{text}");
    }

    #[test]
    fn an_idle_session_says_so_without_inventing_work() {
        let mut ctx = sample();
        ctx.turn_elapsed_ms = None;
        ctx.turn_idle_ms = None;
        ctx.wait_on = None;
        ctx.active_tools.clear();
        ctx.background_tasks.clear();
        let text = render(&ctx);
        assert!(text.contains("空闲"), "{text}");
        assert!(text.contains("后台任务: 无"), "{text}");
        assert!(!text.contains("运行中"), "{text}");
    }

    #[test]
    fn a_long_tail_keeps_the_newest_lines_and_stays_bounded() {
        let mut tail = String::new();
        for i in 0..5000 {
            tail.push_str(&format!("line-{i:05}\n"));
        }
        let mut ctx = sample();
        ctx.background_tasks[0].output_tail = tail;
        let text = render(&ctx);
        assert!(text.contains("line-04999"), "{text}");
        assert!(text.contains("前段已省略"), "{text}");
        assert!(text.len() < 16 * 1024, "injected context must stay small");
    }
}
