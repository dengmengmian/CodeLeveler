#!/usr/bin/env python3
"""Crash-recovery dogfood: the real binary, a real daemon, a real PTY and the
real provider.

The peer-owned `pty_dogfood.py` owns the PTY/screen harness and the reconnect
checks it already ships. This file imports it read-only and adds the three
scenarios the recovery programme still owes:

  A  a foreground tool call is interrupted by SIGKILL; the TUI keeps the user's
     refused input, /recover acknowledge releases the gate without replaying the
     call, and the next real model turn can quote the recovery context.
  B  background lifetime reconciliation: an execution-host task that finishes
     while the runtime is down is reported Completed; a goal-lifetime local
     task whose runtime died is reported Lost, never Completed.
  C  the connection lifecycle during normal use (boot, new session, session
     switch, a long tool call) and one controlled restart: every break in the
     client log must be followed by a reconnect that names the runtime before
     and after.

Every fault here is injected by killing a real process; everything else is a
real turn against the configured provider.

Usage: env -u NODE_OPTIONS python3 scripts/recovery_dogfood.py [A|B|C|all]
"""

import json
import os
import re
import signal
import subprocess
import sys
import time
import unicodedata

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pty_dogfood as pd  # noqa: E402

# The recovery scenarios need a detached runtime: the daemon is the process
# whose death is the fault, and its socket is what the TUI reconnects to.
pd.TRANSPORT = "daemon"
os.makedirs(pd.EVIDENCE, exist_ok=True)

PROVIDER = "real"


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def send(pty_, text: str):
    pty_.write(text)
    time.sleep(0.2)
    pty_.key("\r")


def screen(pty_) -> str:
    return pty_.screen.text()


def compact(text: str) -> str:
    return "".join(text.split())


def cell_col(line: str, char_index: int) -> int:
    """Terminal cell column of a Python string index (wide glyphs are 2)."""
    width = 0
    for ch in line[:char_index]:
        width += 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
    return width


def click_text(pty_, needle: str) -> bool:
    # Bottom-up: the strip cells sit just above the composer, while a
    # transcript row may echo the same words.
    lines = pty_.screen.lines()
    for row in range(len(lines) - 1, -1, -1):
        at = lines[row].find(needle)
        if at >= 0:
            pty_.click(row, cell_col(lines[row], at))
            return True
    return False


def kill_daemons(env) -> list[int]:
    before = pd.daemon_pids(env.repo)
    for pid in before:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    return before


def keep_runtime_down(env, seconds: float) -> list[int]:
    """Kill every replacement runtime until the window closes.

    The TUI respawns `serve` as soon as its subscription fails, so a plain kill
    gives the runtime back in under a second. A task that must finish while the
    runtime is down needs the respawns refused for the whole window.
    """
    killed: list[int] = []
    end = time.time() + seconds
    while time.time() < end:
        for pid in pd.daemon_pids(env.repo):
            try:
                os.kill(pid, signal.SIGKILL)
                killed.append(pid)
            except ProcessLookupError:
                pass
        time.sleep(0.1)
    return killed


def read_lines(path: str) -> list[str]:
    try:
        with open(path) as fh:
            return fh.read().splitlines()
    except FileNotFoundError:
        return []


def runner_log(env) -> str:
    path = os.path.join(env.home, "leveler.log")
    try:
        with open(path) as fh:
            return fh.read()
    except FileNotFoundError:
        return ""


def log_count(env, needle: str) -> int:
    return sum(1 for line in runner_log(env).splitlines() if needle in line)


def pid_field(line: str) -> int | None:
    match = re.search(r"after_pid=(?:Some\()?(\d+)", line)
    return int(match.group(1)) if match else None


def before_pid_field(line: str) -> int | None:
    match = re.search(r"before_pid=(?:Some\()?(\d+)", line)
    return int(match.group(1)) if match else None


def cleanup(env, pty_, tags: list[str]):
    # Stop the client first: a live TUI revives the runtime the moment its
    # subscription breaks, so killing the daemon first races the revival and
    # can leave a fresh process behind after the sweep below.
    pty_.close()
    for tag in tags:
        subprocess.run(["pkill", "-f", tag], capture_output=True)
    for token in ("serve", "execution-host"):
        for pid in pd.repo_processes(env.repo, token):
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def background_inventory(env) -> list[dict]:
    return pd.cli_json(env, ["background", "list", "--json"])


def task_by_tag(inventory: list[dict], tag: str) -> dict | None:
    for task in inventory:
        if tag in " ".join([task.get("program", "")] + task.get("args", [])):
            return task
    return None


# ---------------------------------------------------------------------------
# A — foreground call interrupted, acknowledged, then continued
# ---------------------------------------------------------------------------


def scenario_a(report: pd.Report):
    env = pd.Env(PROVIDER, None, short=True)
    pty_ = pd.boot(report, env, 120, 44)
    tag = "RECOV%d" % os.getpid()
    ran = os.path.join(env.repo, "ran.txt")
    marker = tag + "-MARK"
    probe = tag + "-PROBE"
    try:
        report.check(
            "A",
            "the TUI attached to a detached runtime",
            bool(pd.daemon_pids(env.repo)),
            repr(pd.daemon_pids(env.repo)),
        )
        report.check(
            "A",
            "a first connection reports no interruption",
            "刚才的任务已中断" not in screen(pty_),
            screen(pty_)[-500:],
        )

        send(
            pty_,
            "调用 shell_command 工具，只调用一次，命令为："
            "echo start >> %s; sleep 8; echo end >> %s"
            % (ran, ran)
            + "。这是前台命令，不要设置 background，不要等待别的任务，也不要调用其他工具。",
        )
        began = pd.wait_file(ran, "start", 300)
        report.dump("RECOVERY-A1-command-running", pty_)
        report.check(
            "A",
            "the real model started the command that must not repeat",
            began,
            screen(pty_)[-700:],
        )
        if not began:
            return pty_

        killed = kill_daemons(env)
        restarted = pd.wait_until(
            pty_, lambda text: "刚才的任务已中断" in text, timeout=90
        )
        report.dump("RECOVERY-A2-interrupted", pty_)
        report.check(
            "A",
            "a restart during a tool call is reported as an interruption",
            restarted,
            "killed %r · %s" % (killed, screen(pty_)[-700:]),
        )
        report.check(
            "A",
            "the interrupted call's outcome is shown as unknown",
            "状态未知" in screen(pty_),
            screen(pty_)[-700:],
        )

        finished = pd.wait_file(ran, "end", 60)
        time.sleep(5.0)
        after_crash = read_lines(ran)
        report.check(
            "A",
            "the orphaned command finished on its own",
            finished,
            repr(after_crash),
        )
        report.check(
            "A",
            "the replacement runtime did not rerun the command",
            after_crash == ["start", "end"],
            repr(after_crash),
        )

        # The unresolved call is a gate: the next turn must stop before the
        # model is called, and the user's input must survive it.
        send(pty_, "只回复这个标记，不要任何其他内容：%s" % marker)
        gated = pd.wait_until(
            pty_, lambda text: "任务恢复已暂停" in text, timeout=120
        )
        report.dump("RECOVERY-A3-gated", pty_)
        report.check(
            "A",
            "an interrupted call stops the next turn instead of guessing",
            gated,
            screen(pty_)[-700:],
        )
        report.check(
            "A",
            "the refused input is preserved in the composer",
            marker in pd.composer_text(pty_),
            pd.composer_text(pty_),
        )
        report.check(
            "A",
            "the refused turn executed nothing",
            read_lines(ran) == ["start", "end"],
            repr(read_lines(ran)),
        )

        # /recover lists the evidence; the cursor starts on cancel, so one step
        # down is the acknowledgement.
        pty_.key("\x15")  # Ctrl+U: clear the restored input deliberately
        time.sleep(0.3)
        send(pty_, "/recover")
        overlay = pd.wait_until(
            pty_,
            lambda text: "恢复确认" in text and "已中断（结果未知）" in text,
            timeout=60,
        )
        report.dump("RECOVERY-A4-overlay", pty_)
        report.check(
            "A",
            "/recover lists the interrupted call with its tool and call id",
            overlay,
            screen(pty_)[-900:],
        )
        match = None
        for line in screen(pty_).splitlines():
            match = re.search(r"([A-Za-z_][\w-]*)\s*·\s*([A-Za-z0-9_.:-]{4,})", line)
            if match:
                break
        tool = match.group(1) if match else None
        call_id = match.group(2) if match else None
        report.check(
            "A",
            "the evidence names a specific tool and call",
            bool(tool and call_id),
            repr(screen(pty_)[-900:]),
        )
        if not overlay or not call_id:
            return pty_

        pty_.key("\x1b[B")  # Down: cancel -> confirm
        time.sleep(0.3)
        pty_.key("\r")
        acked = pd.wait_until(
            pty_, lambda text: "已确认恢复" in text, timeout=60
        )
        report.dump("RECOVERY-A5-acknowledged", pty_)
        report.check(
            "A",
            "confirming acknowledges the window",
            acked,
            screen(pty_)[-700:],
        )
        report.check(
            "A",
            "the acknowledgement states the outcome is unknown",
            "结果未知" in screen(pty_),
            screen(pty_)[-700:],
        )
        report.check(
            "A",
            "the acknowledged call stays unknown instead of becoming a failure",
            "状态未知" in screen(pty_) and "个失败" not in screen(pty_),
            screen(pty_)[-700:],
        )
        report.check(
            "A",
            "acknowledging did not replay the call",
            read_lines(ran) == ["start", "end"],
            repr(read_lines(ran)),
        )
        time.sleep(2.0)
        before_probe = screen(pty_)

        # The next turn must reach the model AND that model must have been told
        # about the acknowledged call: it is asked for the call id from the
        # recovery notice, which is nowhere on screen at this point.
        probe_offset = len(pty_.raw)
        send(
            pty_,
            "恢复提示里提到的一个调用 ID 是什么？只回复这个标记和你看到的调用 ID，"
            "不要调用任何工具：%s" % probe,
        )

        def answered() -> bool:
            text = screen(pty_)
            return (
                text.count(probe) >= 2
                and probe not in pd.composer_text(pty_)
                and "Esc 打断" not in text
            )

        replied = pd.wait_until(pty_, lambda _: answered(), timeout=300)
        report.dump("RECOVERY-A6-continued", pty_)
        report.check(
            "A",
            "a new turn after acknowledgement reaches the model and answers",
            replied,
            screen(pty_)[-700:],
        )
        # Search the output produced after the probe: the /recover overlay
        # (which also names the call id) was printed before this offset, and
        # the answer may have scrolled off the 44-row screen.
        after = pty_.raw[probe_offset:].decode("utf-8", "replace")
        report.check(
            "A",
            "the agent received the recovery context (it quotes the call id)",
            call_id in after and call_id not in before_probe,
            "call_id=%r in_reply=%r in_before=%r"
            % (call_id, call_id in after, call_id in before_probe),
        )
        report.check(
            "A",
            "the continued turn still did not replay the command",
            read_lines(ran) == ["start", "end"],
            repr(read_lines(ran)),
        )
        return pty_
    finally:
        cleanup(env, pty_, [tag])


# ---------------------------------------------------------------------------
# B — background lifetime reconciliation across a runtime restart
# ---------------------------------------------------------------------------


def scenario_b(report: pd.Report):
    env = pd.Env(PROVIDER, None, short=True)
    pty_ = pd.boot(report, env, 120, 44)
    hosted_tag = "HOSTED%d" % os.getpid()
    goal_tag = "GOAL%d" % os.getpid()
    hosted_out = os.path.join(env.repo, "hosted.txt")
    try:
        # 1. A runtime-lifetime task belongs to the independent execution host.
        send(
            pty_,
            "调用 run_command 工具，只调用一次：program=\"sh\"，"
            "args=[\"-c\",\"sleep 8; echo %s >> %s\"], background=true，"
            "background_lifetime=\"runtime\"。这是后台任务，不要等待它结束，"
            "也不要调用其他工具。" % (hosted_tag, hosted_out),
        )
        running = pd.wait_until(
            pty_, lambda text: "后台 1" in text, timeout=300
        )
        inventory = background_inventory(env) if running else []
        hosted = task_by_tag(inventory, hosted_tag)
        report.dump("RECOVERY-B1-hosted-running", pty_)
        report.check(
            "B",
            "the real model started a hosted background task",
            bool(hosted and hosted.get("owner") == "execution-host"),
            json.dumps(inventory, ensure_ascii=False)[:900],
        )
        if not hosted:
            return pty_
        hosted_id = hosted["task_id"]

        # Open the jobs list while the task is running: the page is the product
        # surface that must later say how it ended.
        opened = click_text(pty_, "后台 1")
        list_open = opened and pd.wait_until(
            pty_, lambda text: "后台任务" in text and "最近结束" in text, timeout=30
        )
        report.dump("RECOVERY-B2-list-open", pty_)
        report.check(
            "B",
            "the running task's jobs list can be opened from the strip",
            list_open,
            screen(pty_)[-700:],
        )
        if not list_open:
            pty_.key("\x1b")  # Esc back to the conversation
        time.sleep(0.5)

        # 2. Keep the runtime down past the task's exit: the completion now
        #    exists only in the execution host's records, not in a live event.
        killed = kill_daemons(env)
        killed += keep_runtime_down(env, 12.0)
        report.check(
            "B",
            "the hosted task finished while no runtime was alive",
            pd.wait_file(hosted_out, hosted_tag, 30)
            and read_lines(hosted_out) == [hosted_tag],
            "killed %r · %r" % (killed, read_lines(hosted_out)),
        )
        deadline = time.time() + 90
        after: list[int] = []
        while time.time() < deadline and not after:
            after = [pid for pid in pd.daemon_pids(env.repo) if pid not in killed]
            time.sleep(0.2)
        report.check(
            "B",
            "a replacement runtime is alive with a different pid",
            bool(after),
            "killed %r after %r" % (killed, after),
        )

        inventory = background_inventory(env)
        hosted_after = next(
            (task for task in inventory if task.get("task_id") == hosted_id), None
        )
        report.dump("RECOVERY-B3-hosted-completed", pty_)
        report.check(
            "B",
            "the hosted task is reported exited by its own host record",
            bool(hosted_after and hosted_after.get("status") == "exited"),
            json.dumps(inventory, ensure_ascii=False)[:900],
        )
        report.check(
            "B",
            "the command ran exactly once while the runtime was gone",
            read_lines(hosted_out) == [hosted_tag],
            repr(read_lines(hosted_out)),
        )
        if list_open:
            completed_view = pd.wait_until(
                pty_, lambda text: "已完成" in text, timeout=60
            )
            report.dump("RECOVERY-B4-list-completed", pty_)
            report.check(
                "B",
                "the open jobs list shows the task as completed after reconnect",
                completed_view,
                screen(pty_)[-900:],
            )
            pty_.key("\x1b")
            time.sleep(0.5)

        # 3. A goal-lifetime task dies with its runtime: the projection must
        #    report Lost, never Completed, and never silently forget it.
        send(
            pty_,
            "调用 run_command 工具，只调用一次：program=\"sh\"，"
            "args=[\"-c\",\"sleep 600 # %s\"], background=true，"
            "background_lifetime=\"goal\"。这是后台任务，不要等待它结束，"
            "也不要调用其他工具。" % goal_tag,
        )
        local_up = pd.wait_until(
            pty_, lambda text: "后台 1" in text, timeout=300
        )
        inventory = background_inventory(env) if local_up else []
        local = task_by_tag(inventory, goal_tag)
        report.dump("RECOVERY-B5-local-running", pty_)
        report.check(
            "B",
            "the real model started a goal-lifetime task owned by the runtime",
            bool(local and local.get("owner") == "runtime"),
            json.dumps(inventory, ensure_ascii=False)[:900],
        )
        if not local:
            return pty_
        local_id = local["task_id"]

        killed = kill_daemons(env)
        lost_seen = pd.wait_until(
            pty_,
            lambda text: "后台失败" in text or "问题 1" in text,
            timeout=90,
        )
        report.dump("RECOVERY-B6-local-lost", pty_)
        report.check(
            "B",
            "the goal task whose runtime died is surfaced as an open issue",
            lost_seen,
            screen(pty_)[-900:],
        )
        inventory = background_inventory(env)
        still_running = [
            task
            for task in inventory
            if task.get("task_id") == local_id and task.get("status") == "running"
        ]
        report.check(
            "B",
            "the dead runtime's task is not reported running by a live owner",
            not still_running,
            json.dumps(inventory, ensure_ascii=False)[:900],
        )
        report.check(
            "B",
            "the lost task is never reported completed",
            "后台任务已完成" not in screen(pty_),
            screen(pty_)[-900:],
        )

        # The issue page names the task the runtime lost.
        issue = click_text(pty_, "后台失败") or click_text(pty_, "问题 1")
        issue_open = issue and pd.wait_until(
            pty_, lambda text: "问题" in text and goal_tag in text, timeout=60
        )
        report.dump("RECOVERY-B7-lost-issue", pty_)
        report.check(
            "B",
            "the issue page names the lost task, not a completion",
            issue_open,
            screen(pty_)[-900:],
        )
        if issue:
            pty_.key("\x1b")

        # 4. The session still works: a new turn reaches the model.
        marker = "B-CONTINUE-%d" % os.getpid()
        send(pty_, "只回复这个标记，不要任何其他内容：%s" % marker)

        def answered() -> bool:
            text = screen(pty_)
            return text.count(marker) >= 2 and marker not in pd.composer_text(pty_)

        report.check(
            "B",
            "the agent can still take a new turn after the reconciliation",
            pd.wait_until(pty_, lambda _: answered(), timeout=300),
            screen(pty_)[-700:],
        )
        return pty_
    finally:
        cleanup(env, pty_, [goal_tag, hosted_tag])


# ---------------------------------------------------------------------------
# C — connection lifecycle during normal use and one controlled restart
# ---------------------------------------------------------------------------


def scenario_c(report: pd.Report):
    env = pd.Env(PROVIDER, None, short=True)
    # `broke` is warn; the reconnect line that names pid before/after is info.
    env.env["RUST_LOG"] = "leveler=info"
    pty_ = pd.boot(report, env, 120, 44)
    first = "C1-%d" % os.getpid()
    second = "C2-%d" % os.getpid()
    third = "C3-%d" % os.getpid()
    fourth = "C4-%d" % os.getpid()
    try:
        def answered(marker: str, timeout: float = 300) -> bool:
            def done() -> bool:
                text = screen(pty_)
                return (
                    text.count(marker) >= 2
                    and marker not in pd.composer_text(pty_)
                )

            return pd.wait_until(pty_, lambda _: done(), timeout=timeout)

        baseline = log_count(env, "event stream broke")
        report.check(
            "C",
            "a cold start owns exactly one runtime for the repo",
            len(pd.daemon_pids(env.repo)) == 1,
            repr(pd.daemon_pids(env.repo)),
        )
        send(pty_, "只回复这个标记，不要任何其他内容：%s" % first)
        report.check(
            "C",
            "a cold-start turn reaches the model with no stream break",
            answered(first) and log_count(env, "event stream broke") == baseline,
            "broke %d -> %d" % (baseline, log_count(env, "event stream broke")),
        )

        # New session: a fresh subscription, not a reconnect.
        send(pty_, "/new")
        pty_.wait_stable(1.0, 30)
        time.sleep(0.5)
        send(pty_, "只回复这个标记，不要任何其他内容：%s" % second)
        report.check(
            "C",
            "creating a session and taking a turn break no stream",
            answered(second) and log_count(env, "event stream broke") == baseline,
            "broke %d -> %d" % (baseline, log_count(env, "event stream broke")),
        )

        # Switch back to the first session and continue it.
        send(pty_, "/sessions")
        sessions_open = pd.wait_until(
            pty_, lambda text: "Enter 打开" in text, timeout=60
        )
        report.dump("RECOVERY-C1-sessions", pty_)
        pty_.key("\x1b[B")
        time.sleep(0.2)
        pty_.key("\r")
        switched = pd.wait_until(
            pty_, lambda text: first in text, timeout=120
        )
        report.dump("RECOVERY-C2-switched", pty_)
        report.check(
            "C",
            "switching to a stored session re-reads it",
            sessions_open and switched,
            screen(pty_)[-700:],
        )
        send(pty_, "只回复这个标记，不要任何其他内容：%s" % third)
        report.check(
            "C",
            "a turn in the switched session breaks no stream",
            answered(third) and log_count(env, "event stream broke") == baseline,
            "broke %d -> %d" % (baseline, log_count(env, "event stream broke")),
        )

        # A long foreground tool call must not be mistaken for a broken stream.
        send(
            pty_,
            "调用 shell_command 工具，只调用一次，命令为：sleep 6; echo %s。"
            "这是前台命令，不要设置 background，不要等待别的任务，也不要调用其他工具。"
            % fourth,
        )
        long_done = pd.wait_until(
            pty_, lambda text: fourth in text and "Esc 打断" not in text, timeout=300
        )
        report.dump("RECOVERY-C3-long-tool", pty_)
        report.check(
            "C",
            "a long foreground tool call keeps the same stream",
            long_done and log_count(env, "event stream broke") == baseline,
            "broke %d -> %d" % (baseline, log_count(env, "event stream broke")),
        )

        # One controlled restart. Each attached subscription (the session-less
        # stream plus one per open session) logs its own break, so the fact to
        # assert is per stream: every break names its reason, and every broken
        # stream comes back with the runtime pid observed on both sides.
        before_pids = pd.daemon_pids(env.repo)
        reconnect_baseline = log_count(env, "event stream reconnected")
        kill_daemons(env)
        deadline = time.time() + 30
        while (
            time.time() < deadline
            and log_count(env, "event stream broke") <= baseline
        ):
            time.sleep(0.1)
        new_break_lines = [
            line
            for line in runner_log(env).splitlines()
            if "event stream broke" in line
        ][baseline:]
        report.check(
            "C",
            "the controlled restart breaks the attached streams",
            len(new_break_lines) >= 1,
            "%d break line(s); first: %s"
            % (
                len(new_break_lines),
                new_break_lines[0] if new_break_lines else "",
            ),
        )
        reasons = [
            line.split("error=", 1)[1].split(" session=", 1)[0].strip()
            for line in new_break_lines
            if "error=" in line
        ]
        report.check(
            "C",
            "every break is logged with its reason",
            len(reasons) == len(new_break_lines) and all(reasons),
            "reasons: %r" % (reasons,),
        )
        deadline = time.time() + 90
        while time.time() < deadline:
            lines = [
                line
                for line in runner_log(env).splitlines()
                if "event stream reconnected" in line
            ]
            if len(lines) - reconnect_baseline >= len(new_break_lines):
                break
            time.sleep(0.2)
        new_reconnects = [
            line
            for line in runner_log(env).splitlines()
            if "event stream reconnected" in line
        ][reconnect_baseline:]
        report.dump("RECOVERY-C4-restarted", pty_)
        report.check(
            "C",
            "the reconnected client observed the runtime come back",
            bool(new_reconnects),
            screen(pty_)[-500:],
        )
        reconnect_line = new_reconnects[-1] if new_reconnects else ""
        # Restored command authority is proven by a turn, not by a banner: an
        # idle restart owes the user no interruption notice.
        fifth = "C5-%d" % os.getpid()
        send(pty_, "只回复这个标记，不要任何其他内容：%s" % fifth)
        report.check(
            "C",
            "a turn after the controlled restart reaches the model",
            answered(fifth),
            screen(pty_)[-700:],
        )
        live_pids = pd.daemon_pids(env.repo)
        report.check(
            "C",
            "the replacement did not leave a duplicate runtime",
            len(live_pids) == 1 and live_pids != before_pids,
            "before %r after %r" % (before_pids, live_pids),
        )
        before_pid = before_pid_field(reconnect_line)
        after_pid = pid_field(reconnect_line)
        report.check(
            "C",
            "the reconnect names a different runtime pid before and after",
            before_pid is not None
            and after_pid is not None
            and before_pid != after_pid
            and (not before_pids or before_pid in before_pids)
            and after_pid in live_pids,
            "before=%r after=%r logged_before=%r live=%r"
            % (before_pid, after_pid, before_pids, live_pids),
        )
        broke_total = log_count(env, "event stream broke") - baseline
        reconnected_total = (
            log_count(env, "event stream reconnected") - reconnect_baseline
        )
        report.check(
            "C",
            "every break in this run has a matching reconnect",
            broke_total == reconnected_total,
            "this restart: broke %d reconnected %d; run totals %d/%d"
            % (
                len(new_break_lines),
                len(new_reconnects),
                broke_total,
                reconnected_total,
            ),
        )
        return pty_
    finally:
        cleanup(env, pty_, [])


SCENARIOS = {"A": scenario_a, "B": scenario_b, "C": scenario_c}


def main() -> int:
    wanted = sys.argv[1].upper() if len(sys.argv) > 1 else "ALL"
    names = ["A", "B", "C"] if wanted == "ALL" else [wanted]
    report = pd.Report("recovery")
    for name in names:
        fn = SCENARIOS.get(name)
        if fn is None:
            print("unknown scenario %r" % name)
            return 2
        print("\n=== scenario %s ===" % name)
        try:
            fn(report)
        except Exception as exc:  # noqa: BLE001 - a crash is a reported failure
            import traceback

            traceback.print_exc()
            report.check(name, "scenario completed", False, repr(exc))
    path = report.save()
    print(
        "\nassertions: %d, failures: %d, blocked: %d"
        % (len(report.results), report.failures, report.blocked)
    )
    print("evidence: %s" % path)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
