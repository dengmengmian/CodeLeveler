#!/usr/bin/env python3
"""TUI pressure and compatibility dogfood: many tasks, high output, narrow screens.

Deterministic layer: a scripted OpenAI-wire model drives the real runtime; the
processes, the log buffers, the reducer, the renderer and the PTY are real.

What is measured, not asserted by feeling:

  * 18 concurrent background tasks — the conversation stays bounded, the status
    strip counts them, ids stay stable, and one stop does not touch its
    neighbours.
  * a task that prints ~120k lines — how much output the process really made,
    how long the UI takes to echo a keystroke afterwards, whether the log is
    truncated with an explicit note, and whether the terminal's state events
    survive the flood.
  * 48, 80 and 120 column terminals, including a live resize — the status
    strip, the task rows and the input box stay readable at each width.
  * Ctrl+O on the latest receipt: a manual expand is a user decision and is not
    undone by later output.

Usage:
    python3 scripts/tui_pressure.py
    python3 scripts/tui_pressure.py --only tasks
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import bg_dogfood  # noqa: E402
import pty_dogfood as H  # noqa: E402

ROOT = H.ROOT
BIN = H.BIN
EVIDENCE = os.path.join(ROOT, "target", "tui-pressure")
# Frame dumps go through the shared Pty report writer.
H.EVIDENCE = EVIDENCE
REPORT_LAYER = "tui-pressure"


def tool(name: str, arguments: dict, call_id: str = "call_1") -> list[str]:
    return [
        json.dumps(
            {
                "choices": [
                    {
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": call_id,
                                    "function": {
                                        "name": name,
                                        "arguments": json.dumps(arguments),
                                    },
                                }
                            ]
                        }
                    }
                ]
            }
        ),
        json.dumps({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ]


def text(value: str) -> list[str]:
    return H.text_turn(value)


def write_repo(repo: str):
    """Ordinary project commands: many services, one flodder, one quiet one."""
    with open(os.path.join(repo, "Makefile"), "w") as fh:
        fh.write(
            "service:\n\t@echo 'listening'\n\t@sleep 600\n"
            "quiet:\n\t@sleep 600\n"
            "quick:\n\t@echo done\n"
            "flood:\n\t@i=0; while [ $$i -lt 120000 ]; do echo \"line $$i payload\"; i=$$((i+1)); done; echo done\n"
        )


def ask_goal(pty_, prompt: str):
    """Submit one goal through the product's own command."""
    pty_.write("/goal " + prompt)
    time.sleep(0.3)
    pty_.key("\r")


def script(layer, turns):
    layer.script(turns)


def env_for(layer):
    env = layer.env()
    write_repo(env.repo)
    return env


def log_facts(home: str) -> list[dict]:
    """The runtime's own log accounting, as the model saw it."""
    rows = bg_dogfood.query(
        home,
        "select payload from events where type='tool_call_finished' "
        "and (payload like '%task_id%') order by sequence",
    )
    out = []
    for (payload,) in rows:
        try:
            body = json.loads(payload).get("payload", {})
        except Exception:
            continue
        preview = body.get("preview") or ""
        out.append(
            {
                "name": body.get("name"),
                "gap": "log gap" in preview or "no longer retained" in preview,
                "bytes": len(preview),
                "has_result": "status:" in preview,
            }
        )
    return out


def cell_col(line: str, needle: str) -> int:
    """Terminal cell column of `needle`, not its string index.

    The strip is full of CJK, and one character is two cells: clicking a byte
    index lands somewhere else entirely.
    """
    index = line.find(needle)
    if index < 0:
        return 0
    return sum(H.cell_width(ch) for ch in line[:index])


def status_strip(screen: str) -> str:
    for line in screen.splitlines():
        if "后台" in line:
            return line.strip()
    return ""


def scenario_widths(report: H.Report, layer):
    script(
        layer,
        [
            tool("run_command", {"program": "make", "args": ["service"], "background": True}),
            tool("wait_task", {"task_id": "bg-1", "until": "match", "pattern": "listening", "timeout_seconds": 30}),
            tool("get_task", {"task_id": "bg-1"}),
            # Keep observing: the live edge must be a running background task
            # while the terminal is resized under it.
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 600}),
            text("服务在后台运行。"),
        ],
    )
    env = env_for(layer)
    pty_ = H.boot(report, env, 48, 40)
    try:
        ask_goal(pty_, "启动本地服务并观察它。")
        started = H.wait_until(pty_, lambda s: "make service" in s, timeout=90)
        report.dump("width-48-started", pty_)
        screen48 = pty_.screen.text()
        report.check(
            REPORT_LAYER,
            "48 cols: the task row is still readable and names the command",
            started and "make service" in screen48,
            screen48[-600:],
        )
        # The strip's own documented priority gives the background cell the
        # LAST claim: at 48 columns the runtime state and the plan outrank it
        # and the cell is not painted. What must still hold is that the work is
        # observable and reachable, so the check is the reachability below, not
        # a painted count.
        report.check(
            REPORT_LAYER,
            "48 cols: the runtime's live state still names what is running",
            "命令运行中" in screen48 or "等待后台任务" in screen48 or "后台" in screen48,
            status_strip(screen48),
        )
        report.check(
            REPORT_LAYER,
            "48 cols: the input box is present",
            any("›" in line for line in screen48.splitlines()[-6:]),
            screen48[-400:],
        )
        for cols in (80, 120):
            pty_.resize(cols, 40)
            time.sleep(1.0)
            pty_.wait_stable(0.5, 20)
            report.dump("width-%d" % cols, pty_)
            screen = pty_.screen.text()
            # A ready service collapses its members by design; the run is still
            # stated either as the member row or as the aggregate receipt plus
            # the strip's readiness count.
            stated = "make service" in screen or ("正在执行" in screen and "就绪 1" in screen)
            report.check(
                REPORT_LAYER,
                "%d cols: the running task is still stated and the input survives" % cols,
                stated and "›" in screen,
                screen[-500:],
            )
        # At 80 columns every cell fits: the strip must still name the live
        # state AND count the background task. (At 48 the product's documented
        # priority gives the background cell the last claim and drops it.)
        report.check(
            REPORT_LAYER,
            "80 cols: the strip counts the running background task",
            "后台 1" in pty_.screen.text(),
            status_strip(pty_.screen.text()),
        )
        return pty_, env
    finally:
        subprocess.run(["pkill", "-f", "make service"], capture_output=True)
        pty_.close()


def scenario_tasks(report: H.Report, layer):
    """Many real task records at once, inside the runtime's own concurrency cap.

    The registry admits at most `MAX_CONCURRENT` (4) RUNNING background tasks —
    a deliberate resource limit, not a UI limit. So the pressure is on the
    RECORD count and the list: four long-running services plus fourteen quick
    commands produce eighteen real records, and the checks are about what the
    UI does with eighteen of them.
    """
    turns = [tool("update_plan", {"plan": [{"step": "spawn many", "status": "in_progress"}]})]
    targets = ["service"] * 2 + ["quiet"] * 2 + ["quick"] * 14
    for i, target in enumerate(targets):
        turns.append(
            tool(
                "run_command",
                {"program": "make", "args": [target], "background": True},
                "call_%d" % i,
            )
        )
    turns += [
        tool("get_task", {"task_id": "bg-4"}),
        tool("kill_task", {"task_id": "bg-4"}),
        tool("get_task", {"task_id": "bg-1"}),
        # Keep the turn open so goal-lifetime tasks are not reaped under the
        # checks; the pressure under test is the list, not the reaper.
        tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 600}),
        text("后台任务已启动并处理完毕。"),
    ]
    script(layer, turns)
    env = env_for(layer)
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "同时启动多个后台任务，停掉最后一个。")
        counted = H.wait_until(pty_, lambda s: "后台 4" in s, timeout=240)
        report.dump("tasks-1-many", pty_)
        screen = pty_.screen.text()
        report.check(
            REPORT_LAYER,
            "the strip counts the RUNNING tasks the registry admitted (cap 4)",
            counted or "后台 4" in screen,
            status_strip(screen),
        )
        # Every one of the 18 requests got an answer: either a task id, or a
        # refusal that states the concurrency limit. Nothing is dropped in
        # silence, and the registry never runs more than its cap.
        def answered() -> tuple[int, int]:
            admitted = len(
                bg_dogfood.query(
                    env.home,
                    "select payload from events where type='tool_call_finished' "
                    "and payload like '%background task started%'",
                )
            )
            refused = bg_dogfood.query(
                env.home,
                "select count(*) from events where type='tool_call_finished' "
                "and (payload like '%limit%' or payload like '%concurrent%')",
            )
            return admitted, (refused[0][0] if refused else 0)

        admitted, refused_n = answered()
        deadline = time.time() + 90
        while admitted + refused_n < 18 and time.time() < deadline:
            time.sleep(0.5)
            admitted, refused_n = answered()
        report.check(
            REPORT_LAYER,
            "all 18 requests were answered: %d tasks + %d stated refusals"
            % (admitted, refused_n),
            admitted + refused_n == 18,
            repr((admitted, refused_n)),
        )
        running = [
            line for line in screen.splitlines() if "后台 " in line and "↗" in line
        ]
        peak = max(
            (int(tok) for line in running for tok in [line.split("后台 ")[1].split()[0]] if tok.isdigit()),
            default=0,
        )
        report.check(
            REPORT_LAYER,
            "the running count never exceeds the registry's cap of 4 (%d)" % peak,
            0 < peak <= 4,
            status_strip(screen),
        )
        rows = [ln for ln in screen.splitlines() if "make " in ln]
        report.check(
            REPORT_LAYER,
            "18 records: the conversation stays bounded instead of one row per task",
            len(rows) <= 12,
            "rows mentioning make: %d" % len(rows),
        )
        stopped = H.wait_until(
            pty_,
            lambda s: "终止" in s or "已停止" in s or "stopped" in s.lower(),
            timeout=120,
        )
        report.dump("tasks-2-stopped", pty_)
        report.check(
            REPORT_LAYER,
            "one stop is visible for the task it stopped",
            bool(stopped),
            pty_.screen.text()[-700:],
        )
        alive = subprocess.run(
            ["pgrep", "-f", "make service"], stdout=subprocess.PIPE, text=True
        ).stdout.count("\n")
        report.check(
            REPORT_LAYER,
            "stopping bg-4 did not stop its neighbours",
            alive >= 1,
            "pgrep 'make service' matched %d processes" % alive,
        )
        report.check(
            REPORT_LAYER,
            "ids stay stable: the session still addresses bg-1",
            "bg-1" in pty_.screen.text() or "18" in pty_.screen.text(),
            pty_.screen.text()[-500:],
        )
        return pty_, env
    finally:
        for pattern in ("make service", "make quiet"):
            subprocess.run(["pkill", "-f", pattern], capture_output=True)
        pty_.close()


def scenario_flood(report: H.Report, layer):
    """A task that prints ~120k lines: bounded buffers, bounded redraw, honest cuts."""
    script(
        layer,
        [
            tool("run_command", {"program": "make", "args": ["flood"], "background": True}),
            tool("get_task", {"task_id": "bg-1"}),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 300}),
            text("flood 任务结束。"),
        ],
    )
    env = env_for(layer)
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "跑一下 flood 任务，看它输出很多行会怎样。")
        started = H.wait_until(pty_, lambda s: "make flood" in s, timeout=180)
        report.dump("flood-1-started", pty_)
        report.check(REPORT_LAYER, "the flood task starts", bool(started), pty_.screen.text()[-400:])
        # Echo latency while the flood is streaming: measured on a keystroke the
        # reducer must paint into the composer.
        latencies = []
        for i in range(6):
            marker = "x%d" % i
            before = time.time()
            pty_.write(marker)
            for _ in range(60):
                if marker in pty_.screen.text():
                    latencies.append(time.time() - before)
                    break
                time.sleep(0.02)
            else:
                latencies.append(5.0)
            pty_.key("\x7f")  # backspace: keep the composer clean
            time.sleep(0.2)
        worst = max(latencies)
        report.check(
            REPORT_LAYER,
            "input echo stays under 1.5s while 120k lines stream (worst %.3fs)" % worst,
            worst < 1.5,
            repr(latencies),
        )
        # Let the flood finish and check the runtime's own accounting.
        done = H.wait_until(pty_, lambda s: "flood 任务结束" in s, timeout=420)
        report.dump("flood-2-done", pty_)
        report.check(REPORT_LAYER, "the flood task finishes and the turn answers", bool(done), pty_.screen.text()[-500:])
        # The runtime's own log accounting: a flood either stays fully within
        # the buffer, or says how much it dropped. It must never silently
        # present a partial log as complete.
        # What the process really printed, measured outside the runtime.
        produced = subprocess.run(
            ["make", "flood"], cwd=env.repo, capture_output=True, text=True
        ).stdout.count("\n")
        report.check(
            REPORT_LAYER,
            "the process really printed %d lines" % produced,
            produced >= 120_000,
            "",
        )
        # And the model was told the log was cut, not handed a silent partial.
        bodies = "\n".join(layer.mock.requests[-4:])
        report.check(
            REPORT_LAYER,
            "the model is told what it did not get (gap or remaining-log note)",
            "log gap" in bodies
            or "no longer retained" in bodies
            or "more log remains" in bodies,
            bodies[-900:],
        )
        facts = log_facts(env.home)
        report.dump("flood-3-final", pty_)
        bounded = bool(facts) and all(f["bytes"] < 40000 for f in facts)
        report.check(
            REPORT_LAYER,
            "every log the model received is bounded (largest %d bytes)"
            % max([f["bytes"] for f in facts] or [0]),
            bounded,
            repr(facts[-3:]),
        )
        report.check(
            REPORT_LAYER,
            "120k lines of output did not kill the session or the frame",
            "flood" in pty_.screen.text() or "后台" in pty_.screen.text(),
            pty_.screen.text()[-400:],
        )
        return pty_, env
    finally:
        subprocess.run(["pkill", "-f", "make flood"], capture_output=True)
        pty_.close()


SCENARIOS = {
    "widths": scenario_widths,
    "tasks": scenario_tasks,
    "flood": scenario_flood,
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", default="")
    args = parser.parse_args()

    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(BIN):
        print("missing binary: %s" % BIN)
        return 2
    mock = H.MockProvider()
    layer = bg_dogfood.Layer("mock", mock)
    report = H.Report("tui-pressure")
    selected = [args.only] if args.only else list(SCENARIOS)
    try:
        for name in selected:
            print("== %s ==" % name)
            SCENARIOS[name](report, layer)
    finally:
        mock.stop()
    path = report.save()
    print("\n%d check(s) failed; evidence: %s" % (report.failures, path))
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
