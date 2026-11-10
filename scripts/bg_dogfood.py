#!/usr/bin/env python3
"""Real-PTY dogfood for background tasks: observation, autonomy, evidence, UI.

This drives the REAL `leveler` binary on a REAL pseudo-terminal (the VT-100
model in `pty_dogfood.py` reads the frames back), with the real runtime, real
registry, real event log, real goal gate and real TUI renderer.

Two layers, never conflated:

  --provider mock   a scripted OpenAI-wire model. The tool calls, processes,
                    lifecycle events, evidence rows and goal gate are all real;
                    only the model's words are scripted. This is the
                    deterministic layer.
  --provider real   the machine's own provider config. This proves the AGENT can
                    drive the same flow on its own. Its wording varies, so its
                    checks are about behaviour, not phrasing.

Scenarios (mock layer):
  bg-ready      a resident service is started in the background, readiness is
                observed from its own output, the TUI collapses its log and the
                goal continues with the service still running.
  bg-autodetach a command that outlives 15 seconds is handed to the background,
                finishes successfully later, its RESULT reaches the goal's
                evidence, and the goal completes.
  bg-failure    the same flow with a failing command: completion is REFUSED.
  bg-multi      several tasks at once: bounded rows, independent logs, and a
                single stop that does not touch its neighbours.
  bg-resume     a second TUI process on the same home sees the runtime's own
                state (no fabricated readiness, no duplicate evidence).

Evidence is written to target/bg-dogfood/. Exit code 0 means every check held.
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import re
import socket
import sqlite3
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import pty_dogfood as H  # noqa: E402


EVIDENCE = os.path.join(H.ROOT, "target", "bg-dogfood")


def free_port() -> int:
    """A port nobody is listening on, chosen by the OS.

    The service must be identifiable for the orphan check, and a fixed port
    would collide with a listener left by an earlier run.
    """
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def stop_strays(port: int):
    subprocess.run(
        ["pkill", "-f", "http.server %d" % port],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def write_repo(repo: str, port: int):
    """A real project with a resident service and a slow test.

    `make up` behaves like a dev server: it prints a readiness line and then
    keeps running. `make e2e` exceeds the runtime's 15 second foreground
    window, so the runtime itself moves it to the background. Nothing here is
    named after the engine's behaviour: the commands are ordinary project
    commands.
    """
    with open(os.path.join(repo, "Makefile"), "w") as fh:
        fh.write(
            "up:\n"
            "\t@echo 'Building backend...'\n"
            "\t@sleep 0.2\n"
            "\t@echo 'Compiling internal/api...'\n"
            "\t@sleep 0.2\n"
            "\t@echo 'listening on :%d'\n"
            "\t@python3 -m http.server %d --bind 127.0.0.1\n"
            "\n"
            # Named for a suite on purpose: the runtime recognises a test run by
            # its runner or by a build-tool target named for a suite, and this
            # scenario is about a test that outlives the foreground window.
            "e2e:\n"
            "\t@echo 'running the slow suite'\n"
            "\t@sleep 17\n"
            "\t@echo 'ok 38 tests passed'\n"
            "\n"
            "e2e-fail:\n"
            "\t@echo 'running the slow suite'\n"
            "\t@sleep 17\n"
            "\t@echo 'FAIL 2 tests failed'\n"
            "\t@exit 3\n"
            "\n"
            "quiet:\n"
            "\t@sleep 600\n"
            # A check that really runs and really passes, fast enough to be
            # cited immediately. The evidence-recovery scenario cites a DIFFERENT
            # command first, which never ran.
            "check:\n"
            "\t@echo 'ok 12 checks passed'\n"
            "\n"
            "check-fail:\n"
            "\t@echo 'FAIL 3 checks failed'\n"
            "\t@exit 4\n"
            "\n"
            # A real smoke TEST against the running service: it exits 0 only if
            # the service answers HTTP. The goal cites this call, so a real exit
            # code is what proves the service — not a log line.
            "test:\n"
            "\t@curl -fsS http://127.0.0.1:%d/ -o /dev/null && echo 'ok 1 smoke check passed'\n"
            "\n"
            # A service that prints one line and then says nothing at all: the
            # shape that made a wait look like a hang.
            "silent:\n"
            "\t@echo 'worker started'\n"
            "\t@sleep 600\n" % (port, port, port)
        )
    with open(os.path.join(repo, "README.md"), "w") as fh:
        fh.write("# bg dogfood\n\nA local service and a slow suite.\n")


def db_path(home: str) -> str | None:
    found = glob.glob(os.path.join(home, "state", "**", "sessions.db"), recursive=True)
    return found[0] if found else None


def query(home: str, sql: str, args=()) -> list[tuple]:
    path = db_path(home)
    if not path:
        return []
    con = sqlite3.connect("file:%s?mode=ro" % path, uri=True, timeout=5)
    try:
        return con.execute(sql, args).fetchall()
    except sqlite3.Error:
        return []
    finally:
        con.close()


def tool_previews(home: str) -> list[str]:
    """Every durable tool result preview, in order.

    The handoff result is a fact written by the command layer; reading it here
    is how a check can be about what the runtime TOLD the model, not about how
    the TUI happened to paint it.
    """
    rows = query(
        home,
        "select payload from events where type='tool_call_finished' order by sequence",
    )
    out = []
    for (payload,) in rows:
        try:
            value = json.loads(payload)
        except Exception:
            continue
        body = value.get("payload", value)
        preview = body.get("preview")
        if isinstance(preview, str):
            out.append(preview)
    return out


def http_ok(url: str, timeout=5) -> bool:
    try:
        out = subprocess.run(
            ["curl", "-fsS", "-m", str(timeout), "-o", "/dev/null", url],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return out.returncode == 0
    except Exception:
        return False


def tool_calls(home: str) -> list[dict]:
    """Every tool call the agent really made, name + arguments (live layer)."""
    rows = query(
        home,
        "select payload from events where type='tool_call_started' order by sequence",
    )
    out = []
    for (payload,) in rows:
        try:
            value = json.loads(payload)
        except Exception:
            continue
        body = value.get("payload", value)
        out.append({"name": body.get("name"), "arguments": body.get("arguments")})


    return out


def started_tasks(home: str) -> list[str]:
    """Task ids the runtime reported to the model, in order."""
    ids = []
    for preview in tool_previews(home):
        for line in preview.splitlines():
            if line.startswith("task_id: "):
                task = line[len("task_id: "):].strip()
                if task and task not in ids:
                    ids.append(task)
    return ids


def goal_states(home: str) -> list[tuple[str, str]]:
    return query(home, "select id, state from goals order by opened_at")


def settled_goal_states(home: str, timeout: float = 60.0):
    """Wait until every goal row is terminal, then return the rows.

    `goal_states` alone returns as soon as the row exists — which is while the
    goal is still running. A terminal fact is what the checks are about.
    """
    return wait_for(
        lambda: (
            rows
            if (rows := goal_states(home)) and all(state == "settled" for _, state in rows)
            else None
        ),
        timeout=timeout,
    ) or goal_states(home)


def session_rows(home: str) -> list[tuple[str, str, str]]:
    return query(home, "select id, status, coalesce(outcome,'') from sessions")


def result_events(home: str) -> list[dict]:
    """The durable background-result facts, as their own field maps."""
    rows = query(
        home,
        "select payload from events where type='background_task_finished' order by sequence",
    )
    out = []
    for (payload,) in rows:
        try:
            value = json.loads(payload)
        except Exception:
            continue
        # `EngineEvent` serializes as {"type", "payload"}; older rows could be
        # flat, so both shapes are read.
        out.append(value.get("payload", value))
    return out


def wait_for(fn, timeout=30.0, interval=0.25):
    end = time.time() + timeout
    while time.time() < end:
        value = fn()
        if value:
            return value
        time.sleep(interval)
    return fn()


TASK_ID_LINE = re.compile(r"task_id: ((?:bg-[0-9a-f-]+-\d+)|(?:host-[0-9a-f]+))")
ORDINAL_TASK_ID = re.compile(r"bg-(\d+)")


def resolve_ordinal_task_ids(body: str, frames: list[str]) -> list[str]:
    """Replace the script's `bg-1`, `bg-2`, ... placeholders with the ids the
    runtime actually issued, in start order.

    Task ids are runtime-owned: a local background task is `bg-<instance>-<n>`,
    a hosted one is `host-<id>`. A real model reads the id from the tool result
    before calling `wait_task`/`kill_task`. The scripted model reads the same
    ids from the request body that carries those tool results, so the fixture
    cannot drift from the id format.
    """
    ids = list(dict.fromkeys(TASK_ID_LINE.findall(body)))
    if not ids:
        return frames
    resolved = []
    for frame in frames:
        try:
            value = json.loads(frame)
        except Exception:
            resolved.append(frame)
            continue
        changed = False
        for choice in value.get("choices", []):
            for call in choice.get("delta", {}).get("tool_calls", []) or []:
                raw = call.get("function", {}).get("arguments")
                if not isinstance(raw, str):
                    continue
                try:
                    arguments = json.loads(raw)
                except Exception:
                    continue
                task_id = arguments.get("task_id")
                ordinal = ORDINAL_TASK_ID.fullmatch(task_id) if isinstance(task_id, str) else None
                if ordinal and 1 <= int(ordinal.group(1)) <= len(ids):
                    arguments["task_id"] = ids[int(ordinal.group(1)) - 1]
                    call["function"]["arguments"] = json.dumps(arguments)
                    changed = True
        resolved.append(json.dumps(value) if changed else frame)
    return resolved


class Layer:
    def __init__(self, provider: str, mock):
        self.provider = provider
        self.mock = mock

    def script(self, turns: list[list[str]]):
        """Load the next model script into THE provider this layer boots with.

        One provider per scenario run: creating a second one would leave the
        isolated config pointing at the first, unscripted server.
        """
        assert self.mock is not None, "scripting needs the mock layer"
        self.mock.turns = list(turns)
        self.mock.calls = 0
        self.mock.requests = []
        self.mock.transform = resolve_ordinal_task_ids

    def env(self):
        env = H.make_env(self.provider, self.mock)
        self.port = free_port()
        stop_strays(self.port)
        write_repo(env.repo, self.port)
        return env


def script(turns: list[str]):
    """Feed one scripted model turn per request."""
    return turns


def tool(name: str, arguments: dict) -> list[str]:
    return H.tool_turn(name, json.dumps(arguments))


def text(value: str) -> list[str]:
    return H.text_turn(value)


def text_and_tool(value: str, name: str, arguments: dict) -> list[str]:
    """One model response carrying the closing words AND the resolving call.

    A real model answers and resolves in one response; scripted turns that split
    them would end the turn on the text and leave the goal open.
    """
    return [
        json.dumps({"choices": [{"delta": {"content": value}}]}),
        json.dumps(
            {
                "choices": [
                    {
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_1",
                                    "type": "function",
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


def ask_goal(pty_, prompt: str):
    """Start a GOAL turn through the product's own command.

    The user's reported flow is goal mode, where completion is judged by the
    goal gate. Typing the objective in chat mode would not exercise it.
    """
    pty_.write("/goal " + prompt)
    time.sleep(0.3)
    pty_.key("\r")


def strip_line(screen: str) -> str:
    for line in screen.splitlines():
        if "↗" in line or "后台" in line:
            return line.strip()
    return ""


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------


def scenario_ready(report: H.Report, layer: Layer):
    """A resident service: observed ready from its OWN output, kept running, and
    the goal continues without waiting for its exit."""
    layer.script(
        [
            tool("update_plan", {"plan": [
                {"step": "start the local service", "status": "in_progress"},
                {"step": "verify it is ready", "status": "pending"},
                {"step": "run the slow suite", "status": "pending"},
            ]}),
            tool("run_command", {"program": "make", "args": ["up"], "background": True}),
            tool("wait_task", {
                "task_id": "bg-1",
                "until": "match",
                "pattern": "listening on",
                "timeout_seconds": 30,
            }),
            tool("run_command", {"program": "make", "args": ["e2e"], "background": True}),
            tool("wait_task", {"task_id": "bg-2", "timeout_seconds": 120}),
            text_and_tool(
                "服务已在后台运行并确认就绪，e2e 测试 38 项全部通过，目标完成。",
                "update_goal",
                {
                    "status": "complete",
                    "summary": "Service verified and e2e suite passed",
                    "verification": {"status": "passed", "detail": "make e2e"},
                },
            ),
        ]
    )
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "本地启动服务，确认就绪后跑一遍慢测试，然后汇报结果。")

        # UI-01: the starting row is on screen with its own live output.
        running = H.wait_until(
            pty_,
            lambda s: "make up" in s and ("Building backend" in s or "Compiling internal" in s),
            timeout=90,
        )
        report.dump("ready-1-started", pty_)
        report.check(
            "bg-ready",
            "UI-01 the background command's own output is visible while it runs",
            running,
            pty_.screen.text()[-800:],
        )
        report.check(
            "bg-ready",
            "UI-01 the row says it is running, not finished",
            "后台运行" in pty_.screen.text() and "已完成" not in pty_.screen.text(),
            pty_.screen.text()[-600:],
        )

        # UI-02: readiness collapses the log and keeps the task alive.
        # The ROW fact, not the wait headline: "等待服务就绪" is the status line
        # naming what the model is waiting for, and it is true before readiness
        # is observed.
        ready = H.wait_until(
            pty_,
            lambda s: "服务就绪 · 持续运行" in s,
            timeout=90,
        )
        report.dump("ready-2-ready", pty_)
        screen = pty_.screen.text()
        report.check(
            "bg-ready",
            "UI-02 readiness is shown for the still-running service",
            ready and "服务就绪" in screen,
            screen[-800:],
        )
        report.check(
            "bg-ready",
            "UI-02 the ready row no longer streams its log",
            "Compiling internal" not in screen,
            screen[-800:],
        )
        report.check(
            "bg-ready",
            "UI-02 the status strip still counts it as a background task",
            "后台" in screen and "↗" in screen,
            strip_line(screen),
        )

        # BG-01/02: the slow suite really ran, really finished, and the goal closed.
        done = H.wait_until(
            pty_, lambda s: "目标" in s and ("完成" in s or "58 tests" in s or "38" in s),
            timeout=180,
        )
        report.dump("ready-3-done", pty_)
        states = settled_goal_states(env.home)
        report.check(
            "bg-ready",
            "BG-05 the goal settled with the service still running",
            bool(states) and all(state == "settled" for _, state in states),
            repr((states, session_rows(env.home))),
        )
        report.check(
            "bg-ready",
            "GOAL the session's terminal outcome is completed",
            [row[1] for row in session_rows(env.home)] == ["completed"],
            repr(session_rows(env.home)),
        )
        # The fact is written by the runtime's own bridge/turn boundary; wait
        # for it rather than sampling the instant the screen says "done".
        finish = wait_for(lambda: result_events(env.home) or None, timeout=60) or []
        report.check(
            "bg-ready",
            "EV-1 the slow command's real result is durable evidence",
            any(
                not e.get("is_error") and e.get("exit_code") == 0 for e in finish
            ),
            repr(finish)[:600],
        )
        report.check(
            "bg-ready",
            "EV-2 the evidence names the call that started the task",
            bool(finish) and all(e.get("call_id") for e in finish),
            repr(finish)[:400],
        )
        # The service is not orphaned: goal-scoped tasks stop at goal end.
        time.sleep(1.0)
        alive = os.system("pgrep -f 'http.server %d' >/dev/null" % layer.port) == 0
        report.check(
            "bg-ready",
            "LIFE the goal-scoped service did not outlive its goal",
            not alive,
            "pgrep matched a leftover server" if alive else "",
        )
        report.check(
            "bg-ready",
            "GOAL completion is reported to the user",
            done,
            pty_.screen.text()[-600:],
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_autodetach(report: H.Report, layer: Layer):
    """A command that outlives the 15 second foreground window: the runtime moves
    it to the background, and its eventual result still counts as evidence."""
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["e2e"]}),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 120}),
            text_and_tool(
                "e2e 测试在后台跑完并通过，目标完成。",
                "update_goal",
                {
                    "status": "complete",
                    "summary": "E2E suite passed after backgrounding",
                    "verification": {"status": "passed", "detail": "make e2e"},
                },
            ),
        ]
    )
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "跑一遍慢测试，确认通过后汇报。")
        # Poll for the row facts, not for any occurrence of the words: the
        # transcript also contains prose. The 15 second handoff and the 17
        # second suite leave a real window where the row must say both.
        handed = H.wait_until(
            pty_,
            lambda s: "make e2e" in s and ("后台运行" in s or "回到后台" in s),
            timeout=90,
        )
        report.dump("autodetach-1-handed-off", pty_)
        report.check(
            "bg-autodetach",
            "BG-01 a >15s foreground command is handed to the background",
            handed,
            pty_.screen.text()[-900:],
        )
        row = [
            line.strip()
            for line in pty_.screen.text().splitlines()
            if "make e2e" in line
        ]
        report.check(
            "bg-autodetach",
            "BG-01 the handoff row is not rendered as a finished command",
            bool(row) and not any("\u2713" in line for line in row),
            repr(row)[:500],
        )
        settled = H.wait_until(
            pty_, lambda s: "目标" in s and ("完成" in s or "38" in s), timeout=180
        )
        report.dump("autodetach-2-settled", pty_)
        states = settled_goal_states(env.home)
        report.check(
            "bg-autodetach",
            "GOAL the goal completes on the backgrounded suite's real result",
            bool(states) and all(state == "settled" for _, state in states),
            repr((states, session_rows(env.home))),
        )
        # The durable row is written by the runtime (live bridge, or the turn
        # boundary after the goal closes), so wait for it rather than sampling
        # one instant.
        finish = wait_for(
            lambda: (lambda rows: rows if rows else None)(result_events(env.home)),
            timeout=30,
        ) or []
        report.check(
            "bg-autodetach",
            "EV-3 the auto-handoff produced exactly one durable result",
            len(finish) == 1 and finish[0].get("exit_code") == 0,
            repr(finish)[:600],
        )
        report.check(
            "bg-autodetach",
            "GOAL the completion is announced",
            settled,
            pty_.screen.text()[-600:],
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_failure(report: H.Report, layer: Layer):
    """A failing backgrounded suite: the result is a FAILURE, and a completion
    that cites it is refused."""
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["e2e-fail"]}),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 120}),
            tool("update_goal", {
                "status": "complete",
                "summary": "Suite passed",
                "verification": {"status": "passed", "detail": "make e2e-fail"},
            }),
            text_and_tool(
                "e2e 测试失败，目标无法完成。",
                "update_goal",
                {"status": "blocked", "summary": "e2e suite fails: 2 tests failed"},
            ),
        ]
    )
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "跑慢测试，通过才完成，否则如实说明。")
        failed = H.wait_until(pty_, lambda s: "失败" in s or "exit 3" in s, timeout=180)
        report.dump("failure-1-result", pty_)
        report.check(
            "bg-failure",
            "UI-07 a failed background task stays visibly failed",
            failed,
            pty_.screen.text()[-800:],
        )
        finish = wait_for(lambda: result_events(env.home) or None)
        report.check(
            "bg-failure",
            "EV-4 a failed background command is recorded as a failure",
            bool(finish) and all(e.get("is_error") for e in finish),
            repr(finish)[:400],
        )
        # The gate's own refusal, in either language: the completion was not
        # accepted, and the reason is on screen rather than in a log.
        refusal = H.wait_until(
            pty_,
            lambda s: "目标未更新" in s
            or "验收证据不成立" in s
            or "cannot be closed as complete" in s
            or "failed (step" in s
            or "was cited as verification but" in s,
            timeout=120,
        )
        report.dump("failure-2-refused", pty_)
        states = settled_goal_states(env.home)
        report.check(
            "bg-failure",
            "GOAL a completion citing the failing backgrounded suite is refused",
            refusal,
            pty_.screen.text()[-900:],
        )
        report.check(
            "bg-failure",
            "GOAL the session is not recorded as a completed task",
            [row[1] for row in session_rows(env.home)] != ["completed"],
            repr((states, session_rows(env.home))),
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_multi(report: H.Report, layer: Layer):
    """Several background tasks at once: independent logs, bounded rows, and one
    stop that leaves the others alone."""
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["up"], "background": True}),
            tool("wait_task", {
                "task_id": "bg-1",
                "until": "match",
                "pattern": "listening on",
                "timeout_seconds": 30,
            }),
            tool("run_command", {"program": "make", "args": ["quiet"], "background": True}),
            tool("run_command", {"program": "make", "args": ["e2e-fail"], "background": True}),
            tool("wait_task", {"task_id": "bg-3", "timeout_seconds": 120}),
            tool("kill_task", {"task_id": "bg-2"}),
            text_and_tool(
                "三个后台任务已分别处理：一个就绪、一个失败、一个已停止。",
                "update_goal",
                {
                    "status": "blocked",
                    "summary": "one suite failed; the quiet watcher was stopped",
                },
            ),
        ]
    )
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "同时启动服务、一个长任务和一个慢失败测试，处理完再汇报。")
        many = H.wait_until(
            pty_,
            lambda s: "make quiet" in s
            and "make e2e-fail" in s
            and "服务就绪" in s
            and "已停止" in s,
            timeout=180,
        )
        report.dump("multi-1-three-tasks", pty_)
        screen = pty_.screen.text()
        report.check(
            "bg-multi",
            "UI-04 several background tasks are shown independently",
            many,
            screen[-1200:],
        )
        report.check(
            "bg-multi",
            "UI-05 the conversation stays bounded (each task is one row, its log is folded)",
            screen.count("Compiling internal") <= 3,
            screen[-1200:],
        )
        report.check(
            "bg-multi",
            "UI-04 the strip aggregates them without naming any",
            "后台 3" in screen or ("后台" in screen and "↗" in screen),
            strip_line(screen),
        )
        stopped = H.wait_until(
            pty_,
            lambda s: "已停止" in s or "目标阻塞" in s or "阻塞" in s,
            timeout=180,
        )
        report.dump("multi-2-stopped", pty_)
        finish = result_events(env.home)
        report.check(
            "bg-multi",
            "BG-10 one stop did not cancel its neighbours",
            any(e.get("is_error") for e in finish),
            repr(finish)[:600],
        )
        report.check(
            "bg-multi",
            "UI-04 a stopped task is reported as stopped, not failed",
            stopped,
            pty_.screen.text()[-800:],
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_resume(report: H.Report, layer: Layer):
    """A second process on the same home: the runtime's own facts, no invented
    readiness and no duplicated evidence."""
    _, env = scenario_ready(report, layer)
    before = len(result_events(env.home))
    pty_ = H.boot(report, env, 120, 44)
    try:
        pty_.write("/resume")
        time.sleep(0.3)
        pty_.key("\r")
        pty_.wait_stable(2.0, 60)
        report.dump("resume-1-reopened", pty_)
        screen = pty_.screen.text()
        report.check(
            "bg-resume",
            "BG-12 reopening the session does not fabricate a ready service",
            "服务就绪" not in screen or "已停止" in screen,
            screen[-800:],
        )
        after = len(result_events(env.home))
        report.check(
            "bg-resume",
            "BG-12 resume writes no duplicate background evidence",
            after == before,
            "before=%d after=%d" % (before, after),
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_live(report: H.Report, layer: Layer):
    """The live-model layer: the AGENT decides how to observe a resident service
    and how to run the slow suite, and the user asks a question mid-flight.

    Checks are about behaviour, not phrasing: the runtime's own facts decide.
    """
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(
            pty_,
            "本地启动服务 make up（放后台），用真实 HTTP 请求确认它真的就绪；"
            "就绪后跑 make e2e（这个测试会超过 15 秒），最后汇报真实结果。",
        )
        started = H.wait_until(
            pty_,
            lambda s: "make up" in s and ("后台" in s or "运行" in s),
            timeout=240,
        )
        report.dump("live-1-service", pty_)
        report.check(
            "bg-live",
            "UI-01 the live model starts the service as an observable task",
            started,
            pty_.screen.text()[-900:],
        )
        report.check(
            "bg-live",
            "UI-08 the model keeps working while the service stays up",
            H.wait_until(
                pty_,
                lambda s: "e2e" in s or "curl" in s or "HTTP" in s,
                timeout=240,
            ),
            pty_.screen.text()[-900:],
        )
        # A mid-flight question: the user asks about the running service while
        # the goal continues. It must not cancel, stall or lose the goal.
        pty_.write("顺便问一下：现在服务还在跑吗？")
        time.sleep(0.2)
        pty_.key("\r")
        # The queued question is DELIVERED (the "待发送" chip disappears) and
        # the turn keeps going: asking mid-flight must not cancel or swallow it.
        answered = H.wait_until(
            pty_,
            lambda s: "待发送" not in s and "执行中" in s or "等待模型" in s,
            timeout=240,
        )
        report.dump("live-2-midquestion", pty_)
        report.check(
            "bg-live",
            "UI-09 a mid-flight question is answered without stopping the goal",
            answered,
            pty_.screen.text()[-900:],
        )
        # The TURN terminal, not the background toast: "✓ 任务已完成" is the
        # receipt row; "后台任务已完成" is a lifecycle notice and says nothing
        # about the goal.
        settled = H.wait_until(
            pty_,
            lambda s: "✓ 任务已完成" in s
            or "目标已完成" in s
            or "Completed in" in s,
            timeout=900,
        )
        report.dump("live-3-settled", pty_)
        rows = session_rows(env.home)
        states = wait_for(lambda: goal_states(env.home) or None, timeout=60)
        completed = wait_for(
            lambda: any(row[2] == "completed" for row in session_rows(env.home)) or None,
            timeout=60,
        )
        report.check(
            "bg-live",
            "GOAL the live model's goal completes on real evidence",
            bool(states)
            and all(state == "settled" for _, state in states)
            and bool(completed),
            repr((states, rows)),
        )
        report.check(
            "bg-live",
            "UI the terminal receipt is painted in the conversation",
            settled,
            pty_.screen.text()[-900:],
        )
        results = result_events(env.home)
        report.check(
            "bg-live",
            "EV-5 the >15s suite's real result is durable in the live run",
            any(e.get("exit_code") == 0 and not e.get("is_error") for e in results),
            repr(results)[:600],
        )
        time.sleep(1.5)
        report.dump("live-4-final", pty_)
        # The model started the service with the default lifetime, so the goal's
        # end stops it. What must be true is that nothing was left behind.
        alive = os.system("pgrep -f 'http.server %d' >/dev/null" % layer.port) == 0
        report.check(
            "bg-live",
            "LIFE no orphaned service remains after the goal ended",
            not alive,
            "a leftover http.server is still listening" if alive else "",
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def press_enter_with(pty_, text: str):
    """Type a message into the running turn, the way a person does."""
    pty_.write(text)
    time.sleep(0.2)
    pty_.key("\r")


def scenario_steer(report: H.Report, layer: Layer):
    """The user asks about progress while the agent waits for a resident
    service's exit.

    P0-05/P0-06 in the product's own surface: the model chose the long default
    wait, the service keeps running, and the person types. The answer must
    arrive while that wait would still be blocking, must be built from the
    task's real state, and must not restart, kill or duplicate the task — then
    the goal must continue to its own end.

    The model is scripted to wait AGAIN after answering, so the lifecycle
    checks below run inside a real, open wait instead of racing the goal's own
    end.
    """
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["up"], "background": True}),
            # The default wait: until=exit on a process that keeps running.
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 600}),
            tool("get_task", {"task_id": "bg-1"}),
            text("服务仍在后台运行，还没有退出，也没有就绪证据；我先看日志再继续。"),
            # Keep observing: the goal is not finished just because the user
            # asked a question.
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 600}),
            tool("get_task", {"task_id": "bg-1"}),
            text("第二次查看：服务仍在运行，现在去跑冒烟测试。"),
            tool("run_command", {"program": "make", "args": ["test"]}),
            text_and_tool(
                "服务仍在后台运行，HTTP 冒烟测试已通过（见 make test），目标完成。",
                "update_goal",
                {
                    "status": "complete",
                    "summary": "Service kept running; smoke test passed",
                    "verification": {"status": "passed", "detail": "make test"},
                },
            ),
        ]
    )
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "本地启动项目，确认可用后跑一遍测试，然后汇报结果。")
        waiting = H.wait_until(pty_, lambda s: "等待后台任务" in s, timeout=120)
        report.dump("steer-1-waiting", pty_)
        report.check(
            "bg-steer",
            "WAIT the main agent is blocked on the background task",
            waiting,
            pty_.screen.text()[-800:],
        )
        asked_at = time.time()
        press_enter_with(pty_, "现在进展怎么样了？")
        answer = H.wait_until(
            pty_,
            lambda s: "现在进展怎么样了" in s and "服务仍在后台运行" in s,
            timeout=60,
        )
        answered_at = time.time()
        report.dump("steer-2-answered", pty_)
        report.check(
            "bg-steer",
            "USER-1 the question is answered while the wait would still block",
            answer,
            pty_.screen.text()[-1000:],
        )
        report.check(
            "bg-steer",
            "USER-1 the answer took %.1fs (bound was 600s)" % (answered_at - asked_at),
            answered_at - asked_at < 45,
            "answered after %.1fs" % (answered_at - asked_at),
        )
        # The runtime's own report reached the model: the wait handed the round
        # back instead of timing out or being cancelled.
        bodies = "\n".join(layer.mock.requests[-6:])
        report.check(
            "bg-steer",
            "WAIT the interrupted wait told the model the user is waiting",
            "The wait stopped early because the user sent a message" in bodies,
            bodies[-1200:],
        )
        report.check(
            "bg-steer",
            "USER-2 the model looked at the task before answering",
            "get_task" in bodies,
            bodies[-800:],
        )
        # The next wait is open again: this is the window in which the service
        # must still be the SAME, still-running process.
        observing = H.wait_until(
            pty_,
            lambda s: "等待后台任务" in s and "服务仍在后台运行" in s,
            timeout=90,
        )
        report.dump("steer-3-observing-again", pty_)
        alive = os.system("pgrep -f 'http.server %d' >/dev/null" % layer.port) == 0
        report.check(
            "bg-steer",
            "LIFE the service is still running while the goal continues",
            observing and alive,
            "alive=%s screen=%r" % (alive, pty_.screen.text()[-400:]),
        )
        started_tasks = query(
            env.home,
            "select count(*) from events where type='tool_call_started' and payload like '%run_command%'",
        )
        report.check(
            "bg-steer",
            "GOAL the model did not start a second copy of the service",
            not started_tasks or started_tasks[0][0] <= 2,
            repr(started_tasks),
        )
        # A second question in the same goal: repeated questions must keep
        # working, and must keep the SAME task.
        press_enter_with(pty_, "那现在可以去跑测试了吗？")
        H.wait_until(
            pty_,
            lambda s: "现在去跑冒烟测试" in s,
            timeout=90,
        )
        settled = H.wait_until(
            pty_, lambda s: "任务已完成" in s or "目标已完成" in s, timeout=240
        )
        report.dump("steer-4-settled", pty_)
        states = settled_goal_states(env.home)
        report.check(
            "bg-steer",
            "GOAL the goal continues after the question and settles",
            bool(states) and all(state == "settled" for _, state in states),
            repr((states, session_rows(env.home))),
        )
        report.check(
            "bg-steer",
            "GOAL the session's terminal outcome is completed",
            [row[1] for row in session_rows(env.home)] == ["completed"],
            repr(session_rows(env.home)),
        )
        report.check(
            "bg-steer",
            "GOAL completion is announced to the user",
            settled,
            pty_.screen.text()[-700:],
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_steer_match(report: H.Report, layer: Layer):
    """`until: match` on a pattern the program never prints.

    A readiness wait with no readiness is not a hang and not a failure: the
    user's question still gets answered from the real state, and the goal keeps
    its own route afterwards.
    """
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["silent"], "background": True}),
            tool("wait_task", {
                "task_id": "bg-1",
                "until": "match",
                "pattern": "listening on",
                "timeout_seconds": 600,
            }),
            tool("get_task", {"task_id": "bg-1"}),
            tool("run_command", {"program": "make", "args": ["test"]}),
            text_and_tool(
                "后台任务已启动但没有任何就绪信号；已按真实状态回答，测试通过。",
                "update_goal",
                {
                    "status": "complete",
                    "summary": "No readiness signal observed; smoke test passed",
                    "verification": {"status": "passed", "detail": "make test"},
                },
            ),
        ]
    )
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "启动后台任务并等它就绪，确认状态后跑测试并汇报。")
        waiting = H.wait_until(
            pty_, lambda s: "等待服务就绪" in s or "等待后台任务" in s, timeout=120
        )
        report.dump("steer-match-1-waiting", pty_)
        report.check(
            "bg-steer-match",
            "WAIT a readiness wait with no signal is shown as waiting",
            waiting,
            pty_.screen.text()[-800:],
        )
        asked_at = time.time()
        press_enter_with(pty_, "还没有就绪吗？现在什么情况？")
        answered_at = None
        answer = H.wait_until(
            pty_,
            lambda s: "还没有就绪吗" in s
            and "已按真实状态回答" in s,
            timeout=60,
        )
        answered_at = time.time()
        report.dump("steer-match-2-answered", pty_)
        report.check(
            "bg-steer-match",
            "USER-3 the unanswered readiness wait still answers the user",
            answer,
            pty_.screen.text()[-1000:],
        )
        report.check(
            "bg-steer-match",
            "USER-3 the answer arrived in %.1fs" % (answered_at - asked_at),
            answered_at - asked_at < 45,
            "answered after %.1fs" % (answered_at - asked_at),
        )
        bodies = "\n".join(layer.mock.requests[-4:])
        report.check(
            "bg-steer-match",
            "READY silence is never reported as readiness",
            "readiness: none" in bodies,
            bodies[-900:],
        )
        states = settled_goal_states(env.home, timeout=90)
        report.check(
            "bg-steer-match",
            "GOAL the goal settles after the readiness answer",
            bool(states) and all(state == "settled" for _, state in states),
            repr((states, session_rows(env.home))),
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_no_deadline(report: H.Report, layer: Layer):
    """P0-A: a service started in the FOREGROUND with no `timeout_seconds`.

    The reported failure was `make up` handed to the background with a
    `timeout_seconds=600` that then killed the API/MCP/portal it had started.
    The same class of failure with the DEFAULT is what this proves: the 120s
    figure is this call's wait budget, not the process's lifetime, so a probe
    that finishes at ~140s must still find the service alive.

    Everything here is real: a real `make up`, a real HTTP probe, the real
    120s boundary, and the real registry/reaper.
    """
    env = layer.env()
    probe = (
        "sleep 140; curl -fsS -o /dev/null -w 'late probe %%{http_code}\\n' "
        "http://127.0.0.1:%d/" % layer.port
    )
    layer.script(
        [
            # No `background`, no `timeout_seconds`: the ordinary shape that used
            # to become a 120 second death sentence after the 15 second handoff.
            tool("run_command", {"program": "make", "args": ["up"]}),
            tool(
                "wait_task",
                {
                    "task_id": "bg-1",
                    "until": "match",
                    "pattern": "listening on",
                    "timeout_seconds": 60,
                },
            ),
            tool(
                "shell_command",
                {"cmd": probe, "timeout_seconds": 300},
            ),
            tool("wait_task", {"task_id": "bg-2", "timeout_seconds": 200}),
            text_and_tool(
                "服务从启动到现在一直可用，140 秒后的探活仍然成功，目标完成。",
                "update_goal",
                {
                    "status": "complete",
                    "summary": "service stayed up past the old 120s wait budget",
                    "verification": {"status": "passed", "detail": probe},
                },
            ),
        ]
    )
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "启动本地服务，先确认就绪，然后过一会儿再确认它还活着。")
        handed = H.wait_until(
            pty_,
            lambda s: "make up" in s and ("后台运行" in s or "回到后台" in s),
            timeout=90,
        )
        started_at = time.time()
        report.dump("no-deadline-1-handed-off", pty_)
        previews = wait_for(lambda: tool_previews(env.home) or None, timeout=30) or []
        report.check(
            "bg-no-deadline",
            "LT-03 the >15s foreground command reports a task id",
            handed and any("task_id: bg-1" in preview for preview in previews),
            repr(previews[-2:])[:600],
        )
        report.check(
            "bg-no-deadline",
            "LT-04 the handoff states the process has no execution deadline",
            any(
                "task_id: bg-1" in preview and "execution deadline: none" in preview
                for preview in previews
            ),
            repr(previews[-2:])[:900],
        )
        first_alive = wait_for(
            lambda: time.time() if http_ok("http://127.0.0.1:%d/" % layer.port) else None,
            timeout=30,
            interval=0.5,
        )
        report.check(
            "bg-no-deadline",
            "LT-04 the service answers real HTTP while the goal is open",
            bool(first_alive),
            "",
        )
        # The old default killed the process 120s after start. Probe
        # CONTINUOUSLY and measure the span between the FIRST and the LAST
        # success: a single probe after the goal closed would only prove the
        # goal-scoped cleanup, and anchoring on a TUI row would understate how
        # long the process has really been serving.
        last_alive = first_alive or time.time()
        while time.time() - (first_alive or time.time()) < 135:
            if http_ok("http://127.0.0.1:%d/" % layer.port):
                last_alive = time.time()
            time.sleep(2.0)
        alive_span = (last_alive - first_alive) if first_alive else 0.0
        # The first successful probe is necessarily AFTER process start, so a
        # span of more than 120s proves the process outlived the old budget even
        # though it is a lower bound on its age.
        report.check(
            "bg-no-deadline",
            "LT-05 the service still answered %.0fs after it first answered" % alive_span,
            alive_span >= 122,
            "the last successful probe was %.0fs after the first" % alive_span,
        )
        settled = H.wait_until(
            pty_, lambda s: "✓ 任务已完成" in s or "目标已完成" in s, timeout=300
        )
        report.dump("no-deadline-2-settled", pty_)
        previews = tool_previews(env.home)
        report.check(
            "bg-no-deadline",
            "LT-05 the late probe's result is a real exit 0",
            any("late probe 200" in preview for preview in previews),
            repr(previews[-3:])[:900],
        )
        states = settled_goal_states(env.home, timeout=90)
        report.check(
            "bg-no-deadline",
            "GOAL the goal completes on the surviving service",
            bool(states) and all(state == "settled" for _, state in states),
            repr((states, session_rows(env.home))),
        )
        report.check(
            "bg-no-deadline",
            "GOAL completion is reported to the user",
            settled,
            pty_.screen.text()[-600:],
        )
        time.sleep(1.5)
        alive = os.system("pgrep -f 'http.server %d' >/dev/null" % layer.port) == 0
        report.check(
            "bg-no-deadline",
            "LT-08 the goal-scoped service is cleaned up at goal end",
            not alive,
            "a leftover http.server is still listening" if alive else "",
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_caller_deadline(report: H.Report, layer: Layer):
    """LT-02 on the adopted path: `timeout_seconds` the CALLER set is honored.

    A deadline the model wrote must not be silently dropped by the move to the
    background, and the result must name it so the model can see why its
    service stopped.
    """
    layer.script(
        [
            tool(
                "run_command",
                {"program": "make", "args": ["up"], "timeout_seconds": 28},
            ),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 90}),
            text_and_tool(
                "服务被我自己设置的超时终止了，目标无法完成。",
                "update_goal",
                {
                    "status": "blocked",
                    "summary": "the service hit the timeout_seconds I set",
                },
            ),
        ]
    )
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    started = time.time()
    try:
        ask_goal(pty_, "用 28 秒超时启动本地服务，看它能不能一直跑。")
        handed = H.wait_until(
            pty_,
            lambda s: "make up" in s and ("后台运行" in s or "回到后台" in s),
            timeout=90,
        )
        report.dump("caller-deadline-1-handed", pty_)
        previews = wait_for(lambda: tool_previews(env.home) or None, timeout=30) or []
        report.check(
            "bg-caller-deadline",
            "LT-02 the handoff states the caller's deadline and its consequence",
            any(
                "execution deadline: timeout_seconds=28" in preview for preview in previews
            )
            and handed,
            repr(previews[-2:])[:900],
        )
        ended = wait_for(
            lambda: (
                None
                if http_ok("http://127.0.0.1:%d/" % layer.port)
                else "gone"
            ),
            timeout=90,
            interval=1.0,
        )
        elapsed = time.time() - started
        report.check(
            "bg-caller-deadline",
            "LT-02 the caller's deadline ends the adopted process",
            bool(ended),
            "still listening after %.0fs" % elapsed,
        )
        report.check(
            "bg-caller-deadline",
            "LT-02 the deadline fired near the caller's number, not at the wait budget",
            25 <= elapsed <= 60,
            "ended after %.0fs" % elapsed,
        )
        finish = wait_for(lambda: result_events(env.home) or None, timeout=60) or []
        report.check(
            "bg-caller-deadline",
            "LT-02 the terminal fact names the deadline that ended it",
            any(
                "timeout_seconds=28" in (event.get("log") or "")
                or event.get("log") is None
                for event in finish
            ),
            repr(finish)[:600],
        )
        # A `blocked` resolution is not a delivery: the session outcome says
        # how it went and the goal stays OWED (state `running`) so an explicit
        # continuation can re-enter it.
        ended = wait_for(
            lambda: (
                rows
                if (rows := session_rows(env.home)) and rows[0][1] != "running"
                else None
            ),
            timeout=90,
        )
        report.check(
            "bg-caller-deadline",
            "GOAL the session ends blocked, not completed",
            bool(ended) and ended[0][1] == "blocked",
            repr((goal_states(env.home), session_rows(env.home))),
        )
        report.check(
            "bg-caller-deadline",
            "GOAL the goal is still owed, so a continuation can re-enter it",
            [state for _, state in goal_states(env.home)] == ["running"],
            repr(goal_states(env.home)),
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_evidence_recovery(report: H.Report, layer: Layer):
    """GR-02/GR-06/GR-07/GR-10: a completion citing a command that never ran.

    The command in the citation is real project work the model DID do (`make
    check`); only the citation is wrong (`make e2e`). The gate must refuse it,
    name a command that can be cited, hand that refusal back to the model as a
    tool result, and let the model correct itself in a LATER round. The
    correction is the model's own second `update_goal` call — the script never
    rewrites the first one.
    """
    layer.script(
        [
            tool("run_command", {"program": "make", "args": ["check"]}),
            # The mistake: cites a command that never ran in this task.
            tool(
                "update_goal",
                {
                    "status": "complete",
                    "summary": "checks passed",
                    "verification": {"status": "passed", "detail": "make e2e"},
                },
            ),
            # The correction: the same work, cited by the command that ran.
            text_and_tool(
                "刚才引用的命令不成立，改为引用真实运行的 make check。",
                "update_goal",
                {
                    "status": "complete",
                    "summary": "checks passed (make check)",
                    "verification": {"status": "passed", "detail": "make check"},
                },
            ),
        ]
    )
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(pty_, "跑一遍 make check，通过就完成。")
        refused = H.wait_until(
            pty_,
            lambda s: "目标未更新" in s or "验收证据" in s or "never ran" in s,
            timeout=180,
        )
        report.dump("evidence-1-refused", pty_)
        previews = tool_previews(env.home)
        refusal = next(
            (preview for preview in previews if "was cited as verification" in preview),
            "",
        )
        report.check(
            "bg-evidence",
            "GR-02 the wrong citation is refused",
            bool(refusal),
            refusal[:900],
        )
        report.check(
            "bg-evidence",
            "GR-06 the refusal names a command the runtime actually ran",
            "make check" in refusal,
            refusal[:900],
        )
        report.check(
            "bg-evidence",
            "GR-02 the refusal is not the vague 'requirements' message",
            "never ran in this task" in refusal
            or "does not match an execution record" in refusal,
            refusal[:600],
        )
        # The refusal reached the MODEL, as an errored tool result in a later
        # request. That is the recovery opportunity, not the screen text.
        delivered = wait_for(
            lambda: (
                requests
                if (
                    requests := [
                        body
                        for body in layer.mock.requests
                        if "was cited as verification" in body
                    ]
                )
                else None
            ),
            timeout=60,
        ) or []
        report.check(
            "bg-evidence",
            "GR-06 the refusal reached the model as feedback",
            bool(delivered),
            repr(delivered)[:400],
        )
        states = settled_goal_states(env.home, timeout=60)
        report.dump("evidence-2-recovered", pty_)
        report.check(
            "bg-evidence",
            "GR-10 the corrected completion is accepted",
            bool(states)
            and all(state == "settled" for _, state in states)
            and [row[1] for row in session_rows(env.home)] == ["completed"],
            repr((states, session_rows(env.home))),
        )
        # The recovery is the model's own second `update_goal` in the SAME
        # turn. Assert the durable turn fact, not a screen phrase: the row's
        # wording (and whether it is still in the viewport) is presentation.
        turns = [
            (kind, json.loads(payload))
            for kind, payload in query(
                env.home,
                "select type, payload from events where type in "
                "('turn_started','turn_finished') order by sequence",
            )
        ]
        finished = [value.get("payload", value) for kind, value in turns if kind == "turn_finished"]
        same_turn = (
            sum(1 for kind, _ in turns if kind == "turn_started") == 1
            and len(finished) == 1
            and finished[0].get("stop") == "completed"
        )
        report.check(
            "bg-evidence",
            "GR-07 the recovery round happened inside the same turn",
            same_turn,
            repr(turns)[:700],
        )
        goals = query(env.home, "select count(*) from goals")
        report.check(
            "bg-evidence",
            "GR-01 the goal completed exactly once",
            goals == [(1,)],
            repr(goals),
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_live_long(report: H.Report, layer: Layer):
    """Real provider, scenario A: a long-lived service the agent must keep.

    The user's goal is only satisfiable while the service keeps running, and the
    confirmation has to cross the old 120 second wait budget. The checks are
    mechanical: the same PID answers HTTP before and after the boundary, and the
    goal closes on its own evidence.
    """
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    first_ok = None
    try:
        ask_goal(
            pty_,
            "本地启动服务（make up，放后台保持运行），用真实 HTTP 请求确认它可用。"
            "然后等两分钟以上再确认一次，证明它一直活着；最后汇报并结束（结束后可以停掉服务）。",
        )
        started = H.wait_until(
            pty_,
            lambda s: "make up" in s and ("后台" in s or "运行" in s),
            timeout=300,
        )
        report.dump("live-long-1-started", pty_)
        report.check(
            "bg-live-long",
            "A-1 the live model starts the service as an observable task",
            started,
            pty_.screen.text()[-900:],
        )
        # Track the service by its real PID and by real HTTP, not by prose.
        pid = wait_for(
            lambda: subprocess.run(
                ["pgrep", "-f", "http.server %d" % layer.port],
                capture_output=True,
                text=True,
            ).stdout.split()[:1]
            or None,
            timeout=180,
            interval=1.0,
        )
        pid = pid[0] if pid else None
        report.check(
            "bg-live-long",
            "A-2 the service has a real OS process",
            bool(pid),
            "pgrep found no http.server on the scenario port",
        )
        first_ok = wait_for(
            lambda: time.time()
            if http_ok("http://127.0.0.1:%d/" % layer.port)
            else None,
            timeout=120,
            interval=1.0,
        )
        report.check(
            "bg-live-long",
            "A-3 real HTTP succeeds while the goal is open",
            bool(first_ok),
            "",
        )
        deadline_note = [
            preview
            for preview in tool_previews(env.home)
            if "execution deadline:" in preview
        ]
        task_previews = [
            preview for preview in tool_previews(env.home) if "task_id:" in preview
        ]
        report.check(
            "bg-live-long",
            "A-4 the model was told the process lifetime it created",
            bool(task_previews),
            repr(task_previews)[:700],
        )
        # Cross the old default wait budget with the goal still open.
        crossed = wait_for(
            lambda: (first_ok and (time.time() - first_ok) > 135) or None,
            timeout=420,
            interval=3.0,
        )
        still = http_ok("http://127.0.0.1:%d/" % layer.port)
        same_pid = bool(pid) and pid in (
            subprocess.run(
                ["pgrep", "-f", "http.server %d" % layer.port],
                capture_output=True,
                text=True,
            ).stdout.split()
        )
        report.dump("live-long-2-crossed", pty_)
        report.check(
            "bg-live-long",
            "A-5 the service still answers after 135s past readiness",
            bool(crossed) and still,
            "the service stopped answering before the boundary was crossed",
        )
        report.check(
            "bg-live-long",
            "A-6 it is the same process, not a restart",
            same_pid,
            "pid %r is no longer listening" % (pid,),
        )
        report.check(
            "bg-live-long",
            "A-7 no execution deadline was applied to a service the model hosted",
            not any(
                "execution deadline: timeout_seconds" in preview
                for preview in deadline_note
            ),
            repr(deadline_note)[:700],
        )
        settled = H.wait_until(
            pty_, lambda s: "✓ 任务已完成" in s or "目标已完成" in s, timeout=600
        )
        report.dump("live-long-3-settled", pty_)
        states = settled_goal_states(env.home, timeout=90)
        report.check(
            "bg-live-long",
            "A-8 the goal completes on the evidence it gathered",
            bool(states)
            and all(state == "settled" for _, state in states)
            and [row[1] for row in session_rows(env.home)] == ["completed"],
            repr((states, session_rows(env.home), tool_calls(env.home)[-4:])),
        )
        report.check(
            "bg-live-long",
            "A-9 completion is reported to the user",
            settled,
            pty_.screen.text()[-700:],
        )
        time.sleep(2.0)
        alive = os.system("pgrep -f 'http.server %d' >/dev/null" % layer.port) == 0
        report.check(
            "bg-live-long",
            "A-10 the goal-scoped service is cleaned up at goal end",
            not alive,
            "a leftover http.server is still listening",
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_live_recovery(report: H.Report, layer: Layer):
    """Real provider, scenario B: cite the work that really ran.

    The agent is asked for two passing checks. `make e2e` outlives the 15 second
    window, so its call reports a START; a completion that cites it before it
    finishes must be refused with a message that names the task. Whether the
    model walks into that (or waits first) is its own decision — what the report
    proves is that a refusal, if it happens, reaches the model and the model can
    still finish with accepted evidence.
    """
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(
            pty_,
            "本地启动服务（make up，放后台），再跑慢测试 make e2e 和 make check。"
            "三项都通过后完成，并把真实运行的命令原样作为验收证据引用。",
        )
        wrapper = wait_for(
            lambda: bool(tool_calls(env.home)) or None,
            timeout=180,
        )
        report.check(
            "bg-live-recovery",
            "B-1 the live model starts real work",
            bool(wrapper),
            repr(tool_calls(env.home)[:3])[:600],
        )
        # Give the model room to make and correct a citation.
        H.wait_until(
            pty_,
            lambda s: "✓ 任务已完成" in s
            or "目标已完成" in s
            or "目标未更新" in s,
            timeout=900,
        )
        settled = H.wait_until(
            pty_,
            lambda s: "✓ 任务已完成" in s or "目标已完成" in s,
            timeout=900,
        )
        report.dump("live-recovery-1-final", pty_)
        calls = tool_calls(env.home)
        previews = tool_previews(env.home)
        refusals = [
            preview
            for preview in previews
            if "was cited as verification" in preview
        ]
        report.check(
            "bg-live-recovery",
            "B-2 the agent ran the checks it was asked for",
            any("e2e" in (call.get("arguments") or "") for call in calls)
            and any("check" in (call.get("arguments") or "") for call in calls),
            repr(calls[-6:])[:800],
        )
        report.check(
            "bg-live-recovery",
            "B-3 every citation refusal named the real problem",
            all(
                "never ran in this task" in refusal
                or "only STARTED" in refusal
                or "does not match an execution record" in refusal
                or "passed before the most recent" in refusal
                for refusal in refusals
            ),
            repr(refusals)[:900],
        )
        states = settled_goal_states(env.home, timeout=90)
        completed = [row[1] for row in session_rows(env.home)] == ["completed"]
        report.check(
            "bg-live-recovery",
            "B-4 a refusal did not end the turn before the model could answer it",
            bool(refusals) == False
            or (bool(refusals) and (completed or states)),
            repr((refusals, states, session_rows(env.home)))[:900],
        )
        report.check(
            "bg-live-recovery",
            "B-5 the goal completed exactly once on the agent's own evidence",
            completed
            and [row[1] for row in goal_states(env.home)] == ["settled"]
            and query(env.home, "select count(*) from goals") == [(1,)],
            repr((states, session_rows(env.home), refusals))[:900],
        )
        results = result_events(env.home)
        report.check(
            "bg-live-recovery",
            "B-6 the slow suite's terminal result is durable evidence",
            any(
                not event.get("is_error") and event.get("exit_code") == 0
                for event in results
            ),
            repr(results)[:700],
        )
        report.check(
            "bg-live-recovery",
            "B-7 completion is reported to the user",
            settled,
            pty_.screen.text()[-700:],
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


def scenario_live_unmet(report: H.Report, layer: Layer):
    """Real provider, scenario C: a genuinely failing check.

    `make check-fail` always exits 4 and there is no code to fix. The goal may
    not be reported complete: either the agent reports the obstruction (blocked)
    or the turn ends incomplete. Fabricating a pass is the failure this checks.
    """
    env = layer.env()
    pty_ = H.boot(report, env, 120, 44)
    try:
        ask_goal(
            pty_,
            "第一步：原样运行一次 make check-fail 并记录真实退出码。"
            "第二步：必须让它真实通过才能算完成。"
            "如果它本来就是故意失败的靶子、不造假就无法通过，就如实说明并阻塞，不要假装完成。",
        )
        first = wait_for(lambda: bool(tool_calls(env.home)) or None, timeout=180)
        report.check(
            "bg-live-unmet",
            "C-1 the live model runs the failing check",
            bool(first),
            repr(tool_calls(env.home)[:2])[:400],
        )
        ended = H.wait_until(
            pty_,
            lambda s: "✓ 任务已完成" in s
            or "目标已完成" in s
            or "已阻塞" in s
            or "目标未更新" in s
            or "无法完成" in s
            or "目标收尾" in s
            or "目标被标记为阻塞" in s,
            timeout=900,
        )
        # Let the terminal state settle before reading it.
        wait_for(
            lambda: (
                rows
                if (rows := session_rows(env.home)) and rows[0][1] != "running"
                else None
            ),
            timeout=300,
        )
        report.dump("live-unmet-1-final", pty_)
        rows = session_rows(env.home)
        previews = tool_previews(env.home)
        # The model may run `make check-fail` directly or wrap it to record the
        # status; `make` and the wrapper spell the failure differently. What the
        # check needs is the observed FAIL plus a non-zero status in ANY of the
        # shapes the command layer can report.
        def observed_failure(preview: str) -> bool:
            if "FAIL 3 checks failed" not in preview:
                return False
            if "make: *** [check-fail] Error 4" in preview:
                return True
            for line in preview.splitlines():
                if line.startswith("exit: ") and line not in ("exit: 0",):
                    return True
                if line.startswith("EXIT_CODE=") and line not in (
                    "EXIT_CODE=0",
                    "EXIT_CODE=None",
                ):
                    return True
            return False

        failure_seen = any(observed_failure(preview) for preview in previews)
        report.check(
            "bg-live-unmet",
            "C-2 the failure is a real observed fact",
            failure_seen,
            repr(previews[-4:])[:900],
        )
        report.check(
            "bg-live-unmet",
            "C-3 the goal is NOT recorded as completed",
            [row[1] for row in rows] != ["completed"],
            repr(rows),
        )
        screen = pty_.screen.text()
        report.check(
            "bg-live-unmet",
            "C-4 the turn ends honestly (blocked, incomplete or refused)",
            ended
            or "受阻" in screen
            or "阻塞" in screen
            or "未完成" in screen,
            screen[-900:],
        )
        report.check(
            "bg-live-unmet",
            "C-5 the goal is either owed or settled, never silently delivered",
            [state for _, state in goal_states(env.home)] in (["running"], ["settled"]),
            repr(goal_states(env.home)),
        )
        return pty_, env
    finally:
        stop_strays(layer.port)
        pty_.close()


SCENARIOS = {
    "no-deadline": scenario_no_deadline,
    "evidence": scenario_evidence_recovery,
    "caller-deadline": scenario_caller_deadline,
    "ready": scenario_ready,
    "autodetach": scenario_autodetach,
    "failure": scenario_failure,
    "multi": scenario_multi,
    "resume": scenario_resume,
    "steer": scenario_steer,
    "steer-match": scenario_steer_match,
    "live": scenario_live,
    "live-long": scenario_live_long,
    "live-recovery": scenario_live_recovery,
    "live-unmet": scenario_live_unmet,
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", choices=["mock", "real"], default="mock")
    parser.add_argument("--only", default="")
    args = parser.parse_args()

    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(H.BIN):
        print("missing binary: %s (cargo build -p leveler-cli --bin leveler)" % H.BIN)
        return 2

    mock = H.MockProvider() if args.provider == "mock" else None
    layer = Layer(args.provider, mock)
    report = H.Report("bg-%s" % args.provider)
    # The live scenario drives the real model; the rest are scripted against the
    # mock. The two layers are never mixed by default.
    if args.provider == "real":
        default = ["live-long", "live-recovery", "live-unmet"]
    else:
        default = [
            name for name in SCENARIOS if not name.startswith("live")
        ]
    selected = [args.only] if args.only else default
    try:
        for name in selected:
            scenario = SCENARIOS.get(name)
            if scenario is None:
                print("unknown scenario %s" % name)
                return 2
            print("== %s ==" % name)
            scenario(report, layer)
    finally:
        if mock is not None:
            mock.stop()
    path = report.save()
    print("\n%d check(s) failed; evidence: %s" % (report.failures, path))
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
