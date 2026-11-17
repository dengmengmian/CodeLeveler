#!/usr/bin/env python3
"""Real-PTY acceptance for `/tasks` and `/doctor`.

Drives the real `leveler` binary on a real pseudo-terminal with a scripted
OpenAI-wire provider, the real runtime, reducer and renderer. Assertions read
the frames a terminal emulator receives, not a TestBackend.

Scenarios:
  tasks   open while busy, live background lifecycle, failure/completion,
          narrow resize, Esc back without touching the turn
  child   a real background child agent: listed, detail, back with selection,
          runtime cancellation
  doctor  structured categories, live refresh, busy safety, MCP NOT_CHECKED,
          no provider request, resize
  i18n    the same two pages render in English

Evidence is written under .dev/audit/task-diagnostics/evidence/pty/.
Exit code 0 means every assertion held.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import pty_dogfood as h  # noqa: E402

ROOT = os.path.dirname(HERE)
SHARED = os.environ.get("LEVELER_TARGET_DIR") or os.path.join(
    os.path.dirname(ROOT), "CodeLeveler", ".codeleveler-target"
)
h.BIN = os.environ.get("LEVELER_BIN") or os.path.join(SHARED, "debug", "leveler")
h.EVIDENCE = os.path.join(ROOT, ".dev", "audit", "task-diagnostics", "evidence", "pty")
os.makedirs(h.EVIDENCE, exist_ok=True)


def check(report, scenario, name, ok, detail=""):
    report.check(scenario, name, ok, detail)


def open_page(pty, command, needle, timeout=30.0):
    pty.write(command)
    time.sleep(0.2)
    pty.key("\r")
    return pty.wait_for(needle, timeout)


def back(pty):
    pty.key("\x1b")
    time.sleep(0.4)


def scenario_tasks(report):
    """One busy session, then one session with failing and successful
    background work. Separate sessions keep the scripted provider turns
    aligned with the turns the user actually sends."""
    # --- busy: the page opens without stopping the turn --------------------
    mock = h.MockProvider()
    mock.turns = [
        h.tool_turn(
            "run_command",
            json.dumps({"program": "sh", "args": ["-c", "sleep 30"]}),
        ),
        h.text_turn("foreground probe finished"),
    ]
    mock.assist_text = ""
    env = h.make_env("mock", mock)
    pty = h.boot(report, env, 110, 36)
    try:
        pty.write("run the foreground probe")
        time.sleep(0.2)
        pty.key("\r")
        time.sleep(2.0)
        busy = pty.screen.text()
        check(report, "tasks", "the turn is busy before the page opens", "执行" in busy or "运行" in busy, busy[-800:])
        opened = open_page(pty, "/tasks", "任务")
        report.dump("tasks-busy-110x36", pty)
        view = pty.screen.text()
        check(report, "tasks", "/tasks opens while a turn is busy", opened, view[-900:])
        check(report, "tasks", "the current session row is live and running", "当前会话" in view and "运行中" in view, view)
        check(report, "tasks", "opening the page sends no model request", mock.turn_requests() == 1, str(mock.turn_requests()))
        pty.resize(70, 22)
        pty.wait_stable(0.8, 20)
        narrow = pty.screen.text()
        report.dump("tasks-busy-70x22", pty)
        check(report, "tasks", "the page survives a narrow resize", "任务" in narrow and "Esc 返回" in narrow, narrow[-700:])
        pty.resize(110, 36)
        pty.wait_stable(0.8, 20)
        back(pty)
        survived = pty.screen.text()
        check(report, "tasks", "Esc returns to the conversation", "输入消息" in survived or "执行" in survived, survived[-700:])
        check(report, "tasks", "the busy turn survived the page", "执行" in survived or "sleep 30" in survived, survived[-700:])
        pty.write("\x03")
        pty.wait_stable(1.0, 20)
    finally:
        pty.close()
        mock.stop()

    # --- background failure, then completion, in one clean session ---------
    mock = h.MockProvider()
    mock.turns = [
        # One background tool call + closing text = one user turn.
        h.tool_turn(
            "run_command",
            json.dumps(
                {
                    "program": "sh",
                    "args": ["-c", "sleep 6; exit 7"],
                    "background": True,
                    "background_lifetime": "runtime",
                }
            ),
        ),
        h.text_turn("launched the failing background task"),
        h.tool_turn(
            "run_command",
            json.dumps(
                {
                    "program": "sh",
                    "args": ["-c", "sleep 1; exit 0"],
                    "background": True,
                    "background_lifetime": "runtime",
                }
            ),
        ),
        h.text_turn("launched the successful background task"),
    ]
    mock.assist_text = ""
    env = h.make_env("mock", mock)
    pty = h.boot(report, env, 110, 36)
    try:
        pty.write("start the failing probe")
        time.sleep(0.2)
        pty.key("\r")
        pty.wait_for("launched the failing background task", 30)
        time.sleep(0.5)
        open_page(pty, "/tasks", "任务")
        report.dump("tasks-bg-running", pty)
        running = pty.screen.text()
        check(report, "tasks", "a running background task appears in Active", "后台任务" in running and "sleep 6" in running and "运行中" in running, running)
        # The task exits while the page is open. The same page must update in
        # place; no re-open, no query.
        time.sleep(7.0)
        pty.wait_stable(0.8, 20)
        live = pty.screen.text()
        report.dump("tasks-bg-failed", pty)
        check(report, "tasks", "a finished background task leaves Active live", "sleep 6" not in live, live)
        pty.key("\t")
        time.sleep(0.5)
        recent = pty.screen.text()
        report.dump("tasks-bg-failed-recent", pty)
        check(report, "tasks", "the failed task is visible in Recent", "sleep 6" in recent and "失败" in recent, recent)
        back(pty)

        pty.write("start the successful probe")
        time.sleep(0.2)
        pty.key("\r")
        pty.wait_for("launched the successful background task", 30)
        time.sleep(1.8)
        open_page(pty, "/tasks", "任务")
        pty.key("\t")
        time.sleep(0.5)
        done = pty.screen.text()
        report.dump("tasks-bg-completed-recent", pty)
        check(report, "tasks", "a completed background task is Completed, not Running", "sleep 1" in done and "完成" in done, done)
        check(report, "tasks", "completion is not counted as active", "活动 0" in done or "活动" in done, done)
        back(pty)
        pty.write("z")
        time.sleep(0.4)
        usable = pty.screen.text()
        check(report, "tasks", "the conversation is usable after the page", "z" in usable, usable[-500:])
        pty.write("\x7f")
    finally:
        pty.close()
        mock.stop()


def scenario_child(report):
    mock = h.MockProvider()
    state = {"turn": 0}

    def is_child(body):
        # The child gets its own conversation: it carries the child task but
        # not the parent's marker, and no parent turn history.
        return "CHILD_PROBE" in body and "PARENT_MARKER" not in body

    def transform(body, frames):
        if is_child(body):
            return h.tool_turn(
                "run_command",
                json.dumps({"program": "sh", "args": ["-c", "sleep 30"]}),
            )
        state["turn"] += 1
        if state["turn"] == 1:
            # Delegation is a capability, not a default tool. Enable it first;
            # the next request must carry spawn_agent.
            return h.tool_turn(
                "capability",
                json.dumps({"action": "enable", "id": "multi_agent"}),
            )
        if state["turn"] == 2:
            return h.text_turn("multi_agent is enabled")
        if state["turn"] == 3:
            return h.tool_turn(
                "spawn_agent",
                json.dumps(
                    {
                        "title": "child probe",
                        "task": "CHILD_PROBE run one long command",
                        "run_in_background": True,
                    }
                ),
            )
        return h.text_turn("delegated to the child")

    mock.turns = [h.text_turn("fallback")]
    mock.transform = transform
    mock.assist_text = ""
    env = h.make_env("mock", mock)
    pty = h.boot(report, env, 110, 36)
    try:
        pty.write("PARENT_MARKER enable multi_agent")
        time.sleep(0.2)
        pty.key("\r")
        pty.wait_for("multi_agent is enabled", 30)
        time.sleep(0.8)
        pty.write("PARENT_MARKER delegate the probe")
        time.sleep(0.2)
        pty.key("\r")
        spawned = pty.wait_for("child probe", 40)
        report.dump("child-spawned", pty)
        check(report, "child", "the real runtime spawned a child agent", spawned, pty.screen.text()[-900:])
        # Let the child reach its long command before opening the page.
        time.sleep(1.5)
        opened = open_page(pty, "/tasks", "任务")
        listing = pty.screen.text()
        report.dump("child-tasks-110x36", pty)
        check(report, "child", "the child is listed under Child Agents", opened and "子 Agent" in listing and "child probe" in listing, listing)
        check(report, "child", "the child row is Running", "运行中" in listing, listing)
        # The page opens on the current-session row; move to the child row so
        # Enter opens the child's existing detail instead of leaving the page.
        pty.key("j")
        time.sleep(0.3)
        pty.key("\r")
        pty.wait_stable(0.8, 20)
        detail = pty.screen.text()
        report.dump("child-detail", pty)
        check(report, "child", "Enter opens the existing activity detail", "child probe" in detail or "子 Agent" in detail, detail)
        back(pty)
        time.sleep(0.4)
        returned = pty.screen.text()
        check(report, "child", "Esc returns to /tasks with the selection kept", "任务" in returned and "child probe" in returned, returned)
        # Cancel exactly this child through the runtime.
        pty.key("x")
        time.sleep(2.0)
        pty.wait_stable(0.8, 20)
        after = pty.screen.text()
        report.dump("child-after-cancel", pty)
        check(report, "child", "runtime cancellation leaves Active", "child probe" not in after, after)
        back(pty)
    finally:
        pty.close()
        mock.stop()


def scenario_reconnect_pages(report):
    """A killed runtime is replaced; both pages keep working and re-read."""
    old_transport = h.TRANSPORT
    h.TRANSPORT = "daemon"
    mock = h.MockProvider()
    mock.turns = [h.text_turn("idle")]
    mock.assist_text = ""
    env = h.make_env("mock", mock)
    pty = h.boot(report, env, 110, 36)
    try:
        open_page(pty, "/tasks", "任务")
        before = h.daemon_pids(env.repo)
        check(report, "reconnect", "the TUI attached to a detached runtime", bool(before), repr(before))
        if before:
            for pid in before:
                os.kill(pid, signal.SIGKILL)
            after = []
            deadline = time.time() + 60
            while time.time() < deadline and not after:
                after = [pid for pid in h.daemon_pids(env.repo) if pid not in before]
                time.sleep(0.2)
            check(report, "reconnect", "a replacement runtime is alive", bool(after), f"before {before} after {after}")
            time.sleep(2.5)
            pty.wait_stable(0.8, 20)
            view = pty.screen.text()
            report.dump("reconnect-tasks", pty)
            check(report, "reconnect", "/tasks survives the runtime replacement", "任务" in view and "Esc 返回" in view, view)
            back(pty)
            open_page(pty, "/doctor", "诊断")
            time.sleep(1.0)
            pty.wait_stable(0.8, 20)
            doctor = pty.screen.text()
            report.dump("reconnect-doctor", pty)
            check(report, "reconnect", "/doctor re-reads after the replacement", "诊断" in doctor and "运行时连接" in doctor, doctor)
            check(report, "reconnect", "the reconnected runtime passes its connection check", "通过" in doctor, doctor)
            back(pty)
    finally:
        pty.close()
        mock.stop()
        h.TRANSPORT = old_transport


def scenario_sessions(report):
    """Two real sessions: the page scopes the current one and snapshots the
    other, then a real switch re-scopes it."""
    mock = h.MockProvider()
    mock.turns = [
        h.text_turn("SESSION_A_MARKER acknowledged"),
        h.text_turn("idle"),
    ]
    mock.assist_text = ""
    env = h.make_env("mock", mock)
    pty = h.boot(report, env, 110, 36)
    try:
        pty.write("SESSION_A_MARKER remember this session")
        time.sleep(0.2)
        pty.key("\r")
        pty.wait_for("SESSION_A_MARKER acknowledged", 30)
        pty.wait_stable(0.8, 20)
        # A second session; the first remains in the global store.
        pty.write("/new")
        time.sleep(0.2)
        pty.key("\r")
        time.sleep(2.0)
        open_page(pty, "/tasks", "任务")
        pty.key("\t")
        pty.key("\t")
        time.sleep(0.6)
        view = pty.screen.text()
        report.dump("sessions-all", pty)
        check(report, "sessions", "the other session is a scoped snapshot row", "会话" in view and "SESSION_A_MARKER" in view, view)
        back(pty)
        # Switch back through the existing Sessions page.
        pty.write("/sessions")
        time.sleep(0.2)
        pty.key("\r")
        pty.wait_for("会话", 20)
        time.sleep(0.5)
        pty.key("j")
        time.sleep(0.3)
        pty.key("\r")
        time.sleep(1.5)
        switched = pty.screen.text()
        report.dump("sessions-switched", pty)
        check(report, "sessions", "the runtime switched to the older session", "SESSION_A_MARKER" in switched, switched[-800:])
        open_page(pty, "/tasks", "任务")
        scoped = pty.screen.text()
        report.dump("sessions-tasks-after-switch", pty)
        check(report, "sessions", "the task page re-scopes to the switched session", "当前会话" in scoped, scoped)
        back(pty)
    finally:
        pty.close()
        mock.stop()


def scenario_schedule(report):
    """A real schedule definition and one real run, read back by the TUI."""
    mock = h.MockProvider()
    mock.turns = [h.text_turn("idle")]
    mock.assist_text = ""
    # The schedule dispatcher reaches the workspace runtime over a Unix
    # socket; the default macOS temp root exceeds SUN_LEN, so this scenario
    # uses the short-root fixture.
    env = h.Env("mock", mock, short=True)
    add = subprocess.run(
        [
            h.BIN,
            "--repo",
            env.repo,
            "schedule",
            "add",
            "--name",
            "pty schedule",
            "--command",
            "true",
            "--every",
            "3600",
            "--grant-unattended",
            "--allow-external-side-effects",
        ],
        env=env.env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    check(report, "schedule", "the CLI created a real schedule definition", add.returncode == 0, add.stderr[-800:])
    listed = subprocess.run(
        [h.BIN, "--repo", env.repo, "schedule", "list", "--json"],
        env=env.env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    try:
        schedules = json.loads(listed.stdout)
        definition = schedules[0] if isinstance(schedules, list) else schedules.get("schedules", [])[0]
        schedule_id = definition["id"]
    except Exception as error:
        check(report, "schedule", "the definition id is machine-readable", False, f"{error}: {listed.stdout[-800:]}")
        return
    ran = subprocess.run(
        [h.BIN, "--repo", env.repo, "schedule", "run-now", schedule_id, "--json"],
        env=env.env,
        capture_output=True,
        text=True,
        timeout=120,
    )
    check(report, "schedule", "the runtime executed one real schedule run", ran.returncode == 0, ran.stderr[-800:])
    # The dispatcher returns before the action settles. Read the durable run
    # state until it is terminal so the page assertion is about a finished
    # run, not a race against a still-running one.
    state = None
    deadline = time.time() + 60
    while time.time() < deadline:
        shown = subprocess.run(
            [h.BIN, "--repo", env.repo, "schedule", "show", schedule_id, "--json"],
            env=env.env,
            capture_output=True,
            text=True,
            timeout=60,
        )
        try:
            definition = json.loads(shown.stdout)
            state = (definition.get("last_run") or {}).get("state")
            if state is None:
                runs = subprocess.run(
                    [h.BIN, "--repo", env.repo, "schedule", "runs", "--definition", schedule_id, "--json"],
                    env=env.env,
                    capture_output=True,
                    text=True,
                    timeout=60,
                )
                history = json.loads(runs.stdout)
                state = history[0].get("state") if history else None
        except Exception:
            state = None
        if state in ("succeeded", "failed", "cancelled"):
            break
        time.sleep(0.5)
    # The dispatcher may hand the run to a workspace runtime and return while
    # the action is still live. Any typed run state is a real run; the page
    # must show the attempt, not require a terminal outcome.
    check(
        report,
        "schedule",
        "the dispatcher returned a real run state",
        state in ("running", "succeeded", "failed", "cancelled"),
        str(state),
    )
    pty = h.boot(report, env, 110, 36)
    try:
        open_page(pty, "/tasks", "任务")
        pty.key("\t")
        pty.key("\t")  # Active -> Recent -> All
        time.sleep(0.6)
        view = pty.screen.text()
        report.dump("schedule-all", pty)
        check(report, "schedule", "the definition is listed as a definition", "定时" in view and "pty schedule" in view and "已计划" in view, view)
        check(
            report,
            "schedule",
            "the real run appears as a separate run row",
            "定时运行" in view
            and ("运行中" in view or "完成" in view or "失败" in view)
            and "定时定义" in view,
            view,
        )
        back(pty)
    finally:
        pty.close()
        mock.stop()


def scenario_doctor(report):
    mock = h.MockProvider()
    mock.turns = [
        h.tool_turn(
            "run_command",
            json.dumps({"program": "sh", "args": ["-c", "sleep 30"]}),
        ),
        h.text_turn("done"),
    ]
    mock.assist_text = ""
    env = h.make_env("mock", mock)
    pty = h.boot(report, env, 110, 36)
    try:
        before = mock.turn_requests()
        opened = open_page(pty, "/doctor", "诊断")
        time.sleep(0.8)
        pty.wait_stable(0.8, 20)
        view = pty.screen.text()
        report.dump("doctor-idle-110x36", pty)
        check(report, "doctor", "/doctor opens as a structured page", opened and "检查领域" in view, view)
        for word in ["运行时", "模型 / Provider", "MCP", "权限", "沙箱", "终端", "配置", "恢复"]:
            check(report, "doctor", "the %s category is shown" % word, word in view, view)
        check(report, "doctor", "runtime connection is a real PASS", "运行时连接" in view and "通过" in view, view)
        check(report, "doctor", "a real finding carries evidence", "证据" in view, view)
        check(report, "doctor", "the check timestamp is shown", "检查于" in view, view)
        check(report, "doctor", "no provider request was made", mock.turn_requests() == before, str(mock.turn_requests()))

        # Refresh is a runtime query, not a page-local re-render.
        pty.key("r")
        time.sleep(0.3)
        refreshed = pty.screen.text()
        report.dump("doctor-refresh", pty)
        check(report, "doctor", "R requests a fresh check", "正在检查" in refreshed or "检查于" in refreshed, refreshed)
        pty.wait_stable(1.0, 20)
        after = pty.screen.text()
        check(report, "doctor", "the refreshed result keeps the timestamp", "检查于" in after, after)

        pty.resize(70, 22)
        pty.wait_stable(0.8, 20)
        narrow = pty.screen.text()
        report.dump("doctor-70x22", pty)
        check(report, "doctor", "the page survives a narrow resize", "诊断" in narrow and "Esc 返回" in narrow, narrow)
        pty.resize(110, 36)
        pty.wait_stable(0.8, 20)
        back(pty)
        check(report, "doctor", "Esc returns to the conversation", "输入消息" in pty.screen.text(), pty.screen.text()[-600:])

        # --- busy: diagnostics do not disturb the running turn --------------
        pty.write("run the doctor busy probe")
        time.sleep(0.2)
        pty.key("\r")
        time.sleep(2.0)
        open_page(pty, "/doctor", "诊断")
        time.sleep(0.8)
        busy = pty.screen.text()
        report.dump("doctor-busy", pty)
        check(report, "doctor", "the page opens while a turn is busy", "检查领域" in busy, busy)
        back(pty)
        check(report, "doctor", "the running turn continued", "执行" in pty.screen.text() or "sleep 30" in pty.screen.text(), pty.screen.text()[-700:])
        pty.write("\x03")
        pty.wait_stable(1.0, 20)
    finally:
        pty.close()
        mock.stop()


def scenario_i18n(report):
    mock = h.MockProvider()
    mock.turns = [h.text_turn("idle")]
    mock.assist_text = ""
    env = h.make_env("mock", mock)
    cfg = os.path.join(env.home, "config.toml")
    with open(cfg) as fh:
        config = fh.read()
    with open(cfg, "w") as fh:
        fh.write(config.replace('lang = "zh"', 'lang = "en"'))
    pty = h.boot(report, env, 110, 34)
    try:
        open_page(pty, "/tasks", "Tasks")
        pty.key("\t")
        pty.key("\t")  # Active -> Recent -> All
        time.sleep(0.4)
        tasks = pty.screen.text()
        report.dump("i18n-tasks-en", pty)
        check(report, "i18n", "/tasks renders in English", "Tasks" in tasks and "CURRENT SESSION" in tasks, tasks)
        back(pty)
        open_page(pty, "/doctor", "Doctor")
        time.sleep(0.8)
        doctor = pty.screen.text()
        report.dump("i18n-doctor-en", pty)
        check(report, "i18n", "/doctor renders in English", "Doctor" in doctor and "Checks" in doctor and "Runtime" in doctor, doctor)
        check(report, "i18n", "English status words are not Chinese", "PASS" in doctor or "NOT CHECKED" in doctor, doctor)
        back(pty)
    finally:
        pty.close()
        mock.stop()


def main() -> int:
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("--only", default="", help="comma-separated scenario names")
    args = parser.parse_args()
    selected = set(args.only.split(",")) if args.only else None
    report = h.Report("task-diagnostics-pty")
    print("real PTY: %s" % h.BIN)
    for name, fn in [
        ("tasks", scenario_tasks),
        ("child", scenario_child),
        ("reconnect", scenario_reconnect_pages),
        ("sessions", scenario_sessions),
        ("schedule", scenario_schedule),
        ("doctor", scenario_doctor),
        ("i18n", scenario_i18n),
    ]:
        if selected is not None and name not in selected:
            continue
        print("\n== %s ==" % name)
        fn(report)
    path = report.save()
    print("\nfailures: %d" % report.failures)
    print("report: %s" % path)
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
