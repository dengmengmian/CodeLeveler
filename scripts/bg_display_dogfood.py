#!/usr/bin/env python3
"""Real-PTY dogfood for the background-task display contract.

What is real: the `leveler` binary, the PTY, the runtime, the process
lifecycle, the event log, the SQLite records and the TUI renderer. What is
scripted: only the model's words (`--provider mock`), because the scenarios
need deterministic starts, exits and retries. Every tool call the model
"makes" executes for real.

Contract under test:

  * a start is one conversation row plus the strip's live count — never a
    second blue notice;
  * a failure is a record (exit code, duration, output) in the conversation
    and the jobs list — never a strip counter, never a reminder, never an
    acknowledgement interaction;
  * the strip's count is derived from live runtime state and disappears when
    nothing runs.

Scenarios: single-success, single-failure, multi-parallel, long-command,
failure-retry, plan-goal, resume-reconnect.

Evidence is written to target/bg-display-dogfood/. Exit 0 means every check
held.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sqlite3
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import pty_dogfood as H  # noqa: E402
import bg_dogfood as B  # noqa: E402

EVIDENCE = os.path.join(H.ROOT, "target", "bg-display-dogfood")
H.EVIDENCE = EVIDENCE

FORBIDDEN = (
    "已在后台启动",
    "Started in the background",
    "后台失败",
    "background failed",
)


# ---------------------------------------------------------------------------
# Fixtures and scripting
# ---------------------------------------------------------------------------


def write_repo(repo: str, token: str):
    """A real project whose commands the scripted model really runs."""
    with open(os.path.join(repo, "Makefile"), "w") as fh:
        fh.write(
            "quick-ok:\n"
            "\t@echo PREPARING-OK\n"
            "\t@sleep 20\n"
            "\t@echo QUICK-OK-DONE\n"
            "\n"
            "quick-fail:\n"
            "\t@echo PREPARING-FAIL\n"
            "\t@sleep 20\n"
            "\t@echo QUICK-FAIL-DONE\n"
            "\t@exit 7\n"
            "\n"
            "slow-one:\n"
            "\t@echo SLOW-ONE-STARTED\n"
            "\t@python3 -c 'import time; time.sleep(300)' %s-slow-one\n"
            "\n"
            "slow-two:\n"
            "\t@echo SLOW-TWO-STARTED\n"
            "\t@python3 -c 'import time; time.sleep(300)' %s-slow-two\n"
            "\n"
            "long-command:\n"
            "\t@echo LONG-COMMAND-STARTED\n"
            "\t@python3 -c 'import time; time.sleep(300)' %s-long\n"
            % (token, token, token)
        )
    with open(os.path.join(repo, "README.md"), "w") as fh:
        fh.write("# background display dogfood\n")


class Layer:
    def __init__(self, provider: str, mock, token: str):
        self.provider = provider
        self.mock = mock
        self.token = token
        self.last_env = None

    def script(self, turns):
        assert self.mock is not None, "scripting needs the mock layer"
        self.mock.turns = list(turns)
        self.mock.calls = 0
        self.mock.requests = []
        # Task ids are runtime-owned. The script writes `bg-1`, `bg-2`; the
        # provider rewrites them from the ids the runtime actually issued.
        self.mock.transform = B.resolve_ordinal_task_ids

    def env(self):
        env = H.make_env(self.provider, self.mock)
        write_repo(env.repo, self.token)
        self.last_env = env
        return env

    def stop(self):
        subprocess.run(
            ["pkill", "-f", self.token],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )


def tool(name: str, arguments: dict):
    return H.tool_turn(name, json.dumps(arguments))


def collect_metadata(report: H.Report, scenario: str, layer: Layer):
    """Record the identities behind the frames.

    A capture proves what a reader saw. This file proves which session and
    which runtime-issued task produced it, read from the runtime's own store
    rather than from anything the scenario scripted.
    """
    env = layer.last_env
    if env is None:
        return
    db = None
    for root, _dirs, files in os.walk(os.path.join(env.home, "state")):
        if "sessions.db" in files:
            db = os.path.join(root, "sessions.db")
            break
    if db is None:
        report.check(scenario, "EVIDENCE the runtime store exists", False, env.home)
        return
    try:
        con = sqlite3.connect("file:%s?mode=ro" % db, uri=True)
        sessions = [
            {
                "id": row[0],
                "status": row[1],
                "goal": row[2],
                "model": row[3],
                "outcome": row[4],
            }
            for row in con.execute(
                "select id, status, goal, model, outcome from sessions order by created_at"
            )
        ]
        events = []
        for typ, payload in con.execute(
            "select type, payload from events where type like 'background_task%' "
            "order by sequence"
        ):
            inner = json.loads(payload).get("payload", {})
            events.append(
                {
                    "type": typ,
                    "task_id": inner.get("task_id"),
                    "program": inner.get("program"),
                    "args": inner.get("args"),
                    "lifetime": inner.get("lifetime"),
                    "status": inner.get("status"),
                    "exit_code": inner.get("exit_code"),
                    "is_error": inner.get("is_error"),
                }
            )
        con.close()
    except Exception as exc:  # an unreadable store is a failed capture, not a pass
        report.check(scenario, "EVIDENCE the runtime store is readable", False, repr(exc))
        return
    path = os.path.join(EVIDENCE, "meta-%s-%d.json" % (scenario, os.getpid()))
    with open(path, "w") as fh:
        json.dump(
            {
                "scenario": scenario,
                "home": env.home,
                "repo": env.repo,
                "sessions": sessions,
                "background_events": events,
            },
            fh,
            indent=2,
        )
        fh.write("\n")
    report.check(
        scenario,
        "EVIDENCE the session and runtime task identities are recorded",
        bool(sessions) and bool(events),
        path,
    )


def text(value: str):
    return H.text_turn(value)


def ask_async(pty, prompt: str):
    """Submit without waiting for the turn to finish: the scenario needs the
    frames while the task is still live."""
    pty.write(prompt)
    time.sleep(0.2)
    pty.key("\r")


def screen(pty) -> str:
    return pty.screen.text()


def strip_line(pty) -> str:
    """The live-count cell, if any.

    The conversation's own tool row also carries `↗` and `后台运行`, so the
    cell is identified by its count pattern — the same fact the reader sees,
    not the first line that happens to mention the background.
    """
    for line in pty.screen.lines():
        if re.search(r"后台\s*\d+\s*↗", line):
            return line.rstrip()
    return ""


def running_count(pty) -> int:
    line = strip_line(pty)
    found = re.search(r"后台\s*(\d+)", line)
    return int(found.group(1)) if found else 0


def display_col(line: str, index: int) -> int:
    """Character index → terminal column, counting a CJK glyph as two."""
    import unicodedata

    return sum(
        2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
        for ch in line[:index]
    )


def click_strip(pty) -> bool:
    for row, line in enumerate(pty.screen.lines()):
        if re.search(r"后台\s*\d+\s*↗", line):
            marker = line.rindex("↗")
            pty.click(row, display_col(line, marker))
            return True
    return False


def wait_for(predicate, timeout: float, interval: float = 0.2):
    end = time.time() + timeout
    while time.time() < end:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    return predicate()


def expand_conversation(pty) -> bool:
    """Open the folded history row so the tool record is readable.

    A finished run is folded behind `▸ 执行了 N 个命令`; the record is still
    there, this is the reader's own disclosure, not a test-only surface.
    """
    for row, line in enumerate(pty.screen.lines()):
        if "执行了" in line and "▸" in line:
            pty.click(row, line.index("▸"))
            time.sleep(0.5)
            return True
    # No fold means the rows are already the conversation; nothing to open.
    return True


def wait_count(pty, count: int, timeout: float = 60.0) -> bool:
    return wait_for(lambda: running_count(pty) == count, timeout)


def no_legacy_prompts(report: H.Report, scenario: str, pty, label: str):
    """The removed surfaces were painted at least once if their bytes appear
    anywhere in this PTY's stream — a stronger check than the last frame."""
    raw = bytes(pty.raw).decode("utf-8", "replace")
    hits = [word for word in FORBIDDEN if word in raw]
    report.check(
        scenario,
        "%s: no start notice or failure reminder was ever painted" % label,
        not hits,
        repr(hits),
    )
    strip = strip_line(pty)
    report.check(
        scenario,
        "%s: the strip carries no failure glyph and no failure count" % label,
        "⚠" not in strip and "失败" not in strip,
        strip,
    )


def result_events(home: str) -> list[dict]:
    return B.result_events(home)


def previews(home: str) -> list[str]:
    return B.tool_previews(home)


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------


def scenario_single_success(report: H.Report, layer: Layer):
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["quick-ok"], "background": True}),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 60}),
            text("后台任务已经结束，结果见上。"),
        ]
    )
    env = layer.env()
    pty = H.boot(report, env)
    try:
        ask_async(pty, "启动一个后台任务，结束后告诉我。")
        live = wait_count(pty, 1, timeout=60)
        report.dump("success-1-running", pty)
        rows = screen(pty).splitlines()
        start_row = next((line for line in rows if "quick-ok" in line), "")
        report.check(
            "single-success",
            "START the conversation row and the strip count carry the start",
            live and "↗" in start_row and "后台运行" in start_row,
            strip_line(pty) + "\n" + start_row,
        )

        finished = wait_count(pty, 0, timeout=90)
        expanded = expand_conversation(pty)
        home = screen(pty)
        report.dump("success-2-finished", pty)
        report.check(
            "single-success",
            "END the count disappears when the task exits",
            finished,
            strip_line(pty),
        )
        report.check(
            "single-success",
            "END the conversation keeps the command row and its real outcome",
            expanded
            and any(
                "quick-ok" in line and "✓" in line
                for line in home.splitlines()
            )
            and "后台运行" not in home,
            home[-1200:],
        )
        events = result_events(env.home)
        report.check(
            "single-success",
            "END the runtime recorded a successful terminal",
            bool(events) and all(not event.get("is_error") for event in events),
            repr(events)[:600],
        )
        waits = [p for p in previews(env.home) if "status: exited" in p]
        report.check(
            "single-success",
            "AGENT the exit status and the task's id reached the model",
            bool(waits)
            and "exit_code: Some(0)" in waits[0]
            and any(line.startswith("task_id: ") for line in waits[0].splitlines()),
            repr(waits)[:600],
        )
        no_legacy_prompts(report, "single-success", pty, "END")
    finally:
        layer.stop()
        pty.close()


def scenario_single_failure(report: H.Report, layer: Layer):
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["quick-fail"], "background": True}),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 60}),
            text("任务结束，结果并不成功。"),
        ]
    )
    env = layer.env()
    pty = H.boot(report, env)
    try:
        ask_async(pty, "运行失败的后台任务，结束后把结果告诉我。")
        live = wait_count(pty, 1, timeout=60)
        report.dump("failure-1-running", pty)
        report.check(
            "single-failure",
            "START the failing task is live in the same two surfaces",
            live and "quick-fail" in screen(pty),
            screen(pty)[-900:],
        )

        # Open the management record while the task is still live: the jobs
        # list is the place a failure's facts stay queryable.
        opened = click_strip(pty)
        list_ready = wait_for(
            lambda: "后台任务" in screen(pty) and "运行中" in screen(pty),
            20,
        )
        report.dump("failure-2-list-running", pty)
        report.check(
            "single-failure",
            "RECORD the jobs list opens from the live count",
            opened and list_ready,
            screen(pty)[-900:],
        )

        ended = wait_for(
            lambda: "最近结束 · 1" in screen(pty) and "quick-fail" in screen(pty),
            90,
        )
        report.dump("failure-3-list-failed", pty)
        report.check(
            "single-failure",
            "RECORD the failed task is listed as a finished record",
            ended and "EXIT 2" in screen(pty),
            screen(pty)[-900:],
        )
        # Enter focuses the selected row's output.
        pty.key("\r")
        detail = wait_for(
            lambda: "QUICK-FAIL-DONE" in screen(pty)
            and ("Error 7" in screen(pty) or "退出码 2" in screen(pty)),
            20,
        )
        report.dump("failure-4-detail", pty)
        report.check(
            "single-failure",
            "RECORD the detail keeps the real output and exit code",
            detail,
            screen(pty)[-900:],
        )
        pty.key("\x1b", "\x1b")
        time.sleep(0.6)
        expanded = expand_conversation(pty)
        report.dump("failure-5-home", pty)
        home = screen(pty)
        report.check(
            "single-failure",
            "END home shows no failure cell, no reminder and no acknowledgement",
            "后台失败" not in home
            and strip_line(pty) == ""
            and expanded
            and any(
                "quick-fail" in line and "✗" in line and "exit 2" in line
                for line in home.splitlines()
            ),
            strip_line(pty) + "\n" + home[-900:],
        )
        events = result_events(env.home)
        report.check(
            "single-failure",
            "END the runtime recorded the real failure",
            bool(events)
            and all(event.get("is_error") for event in events)
            # `make` turns the recipe's `exit 7` into its own status 2; the
            # real terminal is 2 and the output still carries `Error 7`.
            and any(event.get("exit_code") == 2 for event in events),
            repr(events)[:600],
        )
        waits = [p for p in previews(env.home) if "status: exited" in p]
        report.check(
            "single-failure",
            "AGENT the failure's exit status reached the model",
            bool(waits) and "exit_code: Some(2)" in waits[0],
            repr(waits)[:600],
        )
        no_legacy_prompts(report, "single-failure", pty, "END")
    finally:
        layer.stop()
        pty.close()


def scenario_failure_home(report: H.Report, layer: Layer):
    """A failure with no management interaction.

    The scenario never opens the jobs page: it proves what the conversation
    itself says after a task fails. On the baseline build this is where the
    footer reminder is still painted; on the fixed build the failure exists
    only in its own row and in the runtime's record.
    """
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["quick-fail"], "background": True}),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 60}),
            text("任务结束，结果并不成功。"),
        ]
    )
    env = layer.env()
    pty = H.boot(report, env)
    try:
        ask_async(pty, "运行失败的后台任务，结束后把结果告诉我。")
        live = wait_count(pty, 1, timeout=60)
        report.dump("failure-home-1-running", pty)
        report.check(
            "failure-home",
            "START the failing task is one live row and the running count",
            live and "quick-fail" in screen(pty),
            screen(pty)[-900:],
        )
        # The raw stream positions CJK cells individually, so a phrase can be
        # split by cursor moves. The screen model is what the reader sees.
        report.check(
            "failure-home",
            "START no blue start notice shares the row with the live count",
            "已在后台启动" not in screen(pty),
            screen(pty)[-700:],
        )
        ended = wait_for(
            lambda: running_count(pty) == 0
            and any(event.get("is_error") for event in result_events(env.home)),
            90,
        )
        expand_conversation(pty)
        report.dump("failure-home-2-ended", pty)
        home = screen(pty)
        strip = strip_line(pty)
        report.check(
            "failure-home",
            "END the failure is a recorded row, never a count or a reminder",
            ended
            and strip == ""
            and "后台失败" not in home
            and "⚠" not in strip
            and any(
                "quick-fail" in line and "✗" in line and "exit 2" in line
                for line in home.splitlines()
            ),
            strip + "\n" + home[-900:],
        )
        events = result_events(env.home)
        report.check(
            "failure-home",
            "END the runtime recorded the real failure",
            bool(events)
            and all(event.get("is_error") for event in events)
            # `make` turns the recipe's `exit 7` into its own status 2; the
            # real terminal is 2 and the output still carries `Error 7`.
            and any(event.get("exit_code") == 2 for event in events),
            repr(events)[:600],
        )
        waits = [p for p in previews(env.home) if "status: exited" in p]
        report.check(
            "failure-home",
            "AGENT the failure's exit status reached the model",
            bool(waits) and "exit_code: Some(2)" in waits[0],
            repr(waits)[:600],
        )
        no_legacy_prompts(report, "failure-home", pty, "END")
    finally:
        layer.stop()
        pty.close()


def scenario_multi_parallel(report: H.Report, layer: Layer):
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["slow-one"], "background": True}),
            tool("run_command", {"program": "make", "args": ["slow-two"], "background": True}),
            tool("run_command", {"program": "make", "args": ["long-command"], "background": True}),
            text("三个后台任务正在运行。"),
        ]
    )
    env = layer.env()
    pty = H.boot(report, env)
    try:
        ask_async(pty, "同时启动三个后台任务，保持运行。")
        three = wait_count(pty, 3, timeout=90)
        expand_conversation(pty)
        report.dump("multi-1-three-running", pty)
        text_now = screen(pty)
        report.check(
            "multi-parallel",
            "COUNT the strip reports exactly the three live tasks",
            three,
            strip_line(pty),
        )
        report.check(
            "multi-parallel",
            "ROWS all three commands are one record each",
            all(
                command in text_now
                for command in ("slow-one", "slow-two", "long-command")
            ),
            text_now[-1200:],
        )
        report.check(
            "multi-parallel",
            "ONCE the live state is one strip cell with one count",
            sum(1 for line in text_now.splitlines() if re.search(r"后台\s*3", line)) == 1,
            strip_line(pty),
        )
        no_legacy_prompts(report, "multi-parallel", pty, "RUNNING")
    finally:
        layer.stop()
        pty.close()


def scenario_long_command(report: H.Report, layer: Layer):
    long_script = (
        "import time; print('LONG-COMMAND-STARTED', flush=True); time.sleep(300)"
    )
    layer.script(
        [
            tool(
                "run_command",
                {
                    "program": "python3",
                    "args": ["-c", long_script, layer.token],
                    "background": True,
                    # The inline command is long on purpose; it must outlive the
                    # turn so the live row can be observed, and its argv carries
                    # the scenario token so the fixture can reap it.
                    "background_lifetime": "runtime",
                },
            ),
            text("长时间任务正在后台运行。"),
        ]
    )
    env = layer.env()
    pty = H.boot(report, env)
    try:
        ask_async(pty, "后台运行一个参数很长的命令。")
        live = wait_count(pty, 1, timeout=60)
        expand_conversation(pty)
        report.dump("long-1-running", pty)
        rows = pty.screen.lines()
        row = next((line for line in rows if "python3" in line), "")
        report.check(
            "long-command",
            "DISPLAY the long command is recorded and still one strip line",
            live and "python3" in row,
            row,
        )
        report.check(
            "long-command",
            "DISPLAY no row runs past the 120-column terminal",
            all(len(line) <= 120 for line in rows),
            repr(rows[:4]),
        )
        no_legacy_prompts(report, "long-command", pty, "RUNNING")
    finally:
        layer.stop()
        pty.close()


def scenario_failure_retry(report: H.Report, layer: Layer):
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["quick-fail"], "background": True}),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 60}),
            tool("run_command", {"program": "make", "args": ["quick-ok"], "background": True}),
            tool("wait_task", {"task_id": "bg-2", "timeout_seconds": 60}),
            text("重试已经成功。"),
        ]
    )
    env = layer.env()
    pty = H.boot(report, env)
    try:
        ask_async(pty, "先跑一个会失败的后台任务，再重试一次成功的。")
        first = wait_for(
            lambda: any(event.get("is_error") for event in result_events(env.home))
            and running_count(pty) == 0,
            90,
        )
        report.dump("retry-1-failed", pty)
        report.check(
            "failure-retry",
            "FIRST the failure finished with no strip residue",
            first and "后台失败" not in screen(pty),
            strip_line(pty) + "\n" + screen(pty)[-700:],
        )

        second = wait_for(
            lambda: any(not event.get("is_error") for event in result_events(env.home))
            and running_count(pty) == 0,
            120,
        )
        expand_conversation(pty)
        report.dump("retry-2-succeeded", pty)
        events = result_events(env.home)
        report.check(
            "failure-retry",
            "RETRY the retry's own terminal is a second, successful record",
            second
            and len(events) >= 2
            and any(event.get("is_error") for event in events)
            and any(not event.get("is_error") for event in events),
            repr(events)[:800],
        )
        report.check(
            "failure-retry",
            "RETRY both commands remain in the conversation",
            "quick-fail" in screen(pty) and "quick-ok" in screen(pty),
            screen(pty)[-1200:],
        )
        no_legacy_prompts(report, "failure-retry", pty, "END")
    finally:
        layer.stop()
        pty.close()


def scenario_plan_goal(report: H.Report, layer: Layer):
    layer.script(
        [
            tool(
                "update_plan",
                {
                    "plan": [
                        {"step": "启动后台任务", "status": "in_progress"},
                        {"step": "确认任务仍在运行", "status": "pending"},
                    ]
                },
            ),
            tool("run_command", {"program": "make", "args": ["slow-one"], "background": True}),
            text("后台任务已启动，计划等待确认。"),
        ]
    )
    env = layer.env()
    pty = H.boot(report, env)
    try:
        B.ask_goal(pty, "先更新计划，再启动一个后台任务。")
        live = wait_count(pty, 1, timeout=90)
        plan_ready = wait_for(lambda: "计划" in screen(pty), 60)
        report.dump("plan-1-running", pty)
        report.check(
            "plan-goal",
            "PLAN the plan line and the background count coexist",
            live and plan_ready,
            screen(pty)[-1200:],
        )
        report.check(
            "plan-goal",
            "GOAL the goal context is still the active turn",
            "目标" in screen(pty) or "goal" in screen(pty).lower(),
            screen(pty)[-1200:],
        )
        no_legacy_prompts(report, "plan-goal", pty, "RUNNING")
    finally:
        layer.stop()
        pty.close()


def open_session(pty, needle: str) -> bool:
    """Reopen a specific stored session from the TUI's own list.

    The cursor is placed by clicking the row that names this session's first
    turn; Enter is the list's own open action.
    """
    pty.write("/sessions")
    time.sleep(0.3)
    pty.key("\r")
    listed = wait_for(lambda: needle in screen(pty), 30)
    if not listed:
        return False
    for row, line in enumerate(pty.screen.lines()):
        if needle in line:
            pty.click(row, 6)
            break
    time.sleep(0.3)
    pty.key("\r")
    return True


def scenario_resume_reconnect(report: H.Report, layer: Layer):
    layer.script(
        [
            tool(
                "run_command",
                {
                    "program": "make",
                    "args": ["slow-one"],
                    "background": True,
                    # A crash-reconnect scenario is about a service the host
                    # owns; a goal-lifetime task is stopped with its owner.
                    "background_lifetime": "runtime",
                },
            ),
            text("后台任务正在运行。"),
        ]
    )
    env = layer.env()
    first = H.boot(report, env)
    second = None
    try:
        ask_async(first, "启动一个长时间后台任务。")
        live = wait_count(first, 1, timeout=90)
        report.dump("resume-1-first-client", first)
        report.check(
            "resume-reconnect",
            "FIRST the first client is live",
            live,
            screen(first)[-900:],
        )

        # The client dies; the daemon and the process it hosts do not.
        first.close()
        second = H.boot(report, env)
        opened = open_session(second, "长时间后台任务")
        restore = opened and wait_for(
            lambda: running_count(second) == 1 and "slow-one" in screen(second),
            90,
        )
        report.dump("resume-2-second-client", second)
        report.check(
            "resume-reconnect",
            "RESUME the reopened session shows the same live task",
            restore,
            screen(second)[-1200:],
        )
        report.check(
            "resume-reconnect",
            "RESUME the second client invents no start notice",
            "已在后台启动" not in screen(second),
            screen(second)[-900:],
        )
        report.check(
            "resume-reconnect",
            "RESUME the task is still running, not fabricated as finished",
            not result_events(env.home) and running_count(second) == 1,
            repr(result_events(env.home))[:400],
        )
        no_legacy_prompts(report, "resume-reconnect", second, "RESUME")
    finally:
        if second is not None:
            second.close()
        layer.stop()
        for pid in H.daemon_pids(env.repo):
            try:
                os.kill(pid, 15)
            except OSError:
                pass



SCENARIOS = {
    "single-success": scenario_single_success,
    "single-failure": scenario_single_failure,
    "failure-home": scenario_failure_home,
    "multi-parallel": scenario_multi_parallel,
    "long-command": scenario_long_command,
    "failure-retry": scenario_failure_retry,
    "plan-goal": scenario_plan_goal,
    "resume-reconnect": scenario_resume_reconnect,
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", choices=["mock"], default="mock")
    parser.add_argument("--transport", choices=["in-process", "daemon"], default="daemon")
    parser.add_argument("--only", default="", help="comma-separated scenario names")
    parser.add_argument("--bin", default=None)
    parser.add_argument(
        "--evidence",
        default=None,
        help="evidence directory (default target/bg-display-dogfood)",
    )
    args = parser.parse_args()
    if args.bin:
        H.BIN = args.bin
    H.TRANSPORT = args.transport
    global EVIDENCE
    if args.evidence:
        EVIDENCE = os.path.abspath(args.evidence)
        H.EVIDENCE = EVIDENCE
    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(H.BIN):
        print("missing %s — build it first (cargo build -p leveler-cli)" % H.BIN)
        return 2

    selected = [name for name in (args.only.split(",") if args.only else SCENARIOS) if name]
    report = H.Report(args.provider)
    for name in selected:
        mock = H.MockProvider()
        token = "bgdog-%s-%d" % (name, os.getpid())
        layer = Layer(args.provider, mock, token)
        print("=== scenario %s (%s, %s transport) ===" % (name, args.provider, args.transport))
        try:
            SCENARIOS[name](report, layer)
            collect_metadata(report, name, layer)
        except Exception as exc:  # a crashed scenario is a FAIL, not a skip
            report.check(name, "scenario completed", False, repr(exc))
            import traceback

            traceback.print_exc()
        finally:
            mock.stop()
    path = report.save()
    print(
        "\nassertions: %d, failures: %d, blocked: %d"
        % (len(report.results), report.failures, report.blocked)
    )
    print("evidence: %s" % path)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
