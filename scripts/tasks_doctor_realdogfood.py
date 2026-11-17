#!/usr/bin/env python3
"""Real-provider dogfood for `/tasks` and `/doctor`.

Runs the real `leveler` binary on a real PTY against the machine's configured
provider (default: `deepseek/deepseek-v4.1-flash-1017`) in an isolated repo and
LEVELER_HOME. The task is a small multi-module Python deliverable with real
tests. During the turn the script opens `/tasks` and `/doctor` and reads the
frames; after the turn it runs the project's tests itself.

Delegation is never forced. If the model chooses to delegate, the script
records the child row; if it does not, the deterministic PTY suite in
`tasks_doctor_dogfood.py` covers the child path.

Evidence: .dev/audit/task-diagnostics/evidence/dogfood/
Set LEVELER_REAL_MODEL to override the default model (provider/id).
"""

from __future__ import annotations

import glob
import json
import os
import sqlite3
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
EVIDENCE = os.path.join(ROOT, ".dev", "audit", "task-diagnostics", "evidence", "dogfood")
os.makedirs(EVIDENCE, exist_ok=True)
h.EVIDENCE = EVIDENCE


def load_env_file(path):
    """Load KEY=VALUE / export KEY=VALUE without ever printing a value."""
    if not os.path.exists(path):
        return
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            if line.startswith("export "):
                line = line[len("export ") :]
            if "=" not in line:
                continue
            key, value = line.split("=", 1)
            os.environ.setdefault(key.strip(), value.strip().strip('"').strip("'"))


PROMPT = (
    "请在当前隔离仓库完成一个小型 Python 开发任务：\n"
    "- 包 calc，包含 calc/ops.py（加减乘除）与 calc/cli.py"
    "（执行 `python3 -m calc.cli` 输出 2 + 3 = 5）\n"
    "- tests/test_ops.py 使用 unittest，覆盖除零行为\n"
    "- 运行 `python3 -m unittest discover -s tests -v`，处理真实失败\n"
    "- 最后运行 `python3 -m calc.cli` 确认输出\n"
    "如需委派可自行决定。完成后用简短总结说明交付与验证结果。"
)


def check(report, name, ok, detail=""):
    report.check("dogfood", name, ok, detail)


def durable_session_status(home: str):
    """The runtime's own persisted session status, not a screen guess."""
    databases = glob.glob(os.path.join(home, "state", "projects", "*", "sessions.db"))
    if not databases:
        return None
    database = max(databases, key=os.path.getmtime)
    try:
        connection = sqlite3.connect("file:%s?mode=ro" % database, uri=True)
        row = connection.execute(
            "select status from sessions order by updated_at desc limit 1"
        ).fetchone()
        connection.close()
        return row[0] if row else None
    except Exception:
        return None


def main() -> int:
    load_env_file(os.path.expanduser("~/.leveler/deepseek.env"))
    if not os.environ.get("DEEPSEEK_API_KEY"):
        print("DEEPSEEK_API_KEY is not set; refusing to fake a real-provider run")
        return 2
    report = h.Report("task-diagnostics-dogfood")
    env = h.Env("real", None)
    summary = {
        "bin": h.BIN,
        "repo": env.repo,
        "prompt": PROMPT,
        "models": os.environ.get("LEVELER_REAL_MODEL", "config default"),
        "child_observed": False,
        "doctor_opened": False,
        "turn_terminal": None,
        "tests": None,
        "cli": None,
    }
    pty = h.boot(report, env, 120, 40)
    started = time.time()
    try:
        pty.write(PROMPT)
        time.sleep(0.3)
        pty.key("\r")

        # Let the first model round actually start before opening the page.
        time.sleep(6.0)
        opened = None
        pty.write("/tasks")
        time.sleep(0.2)
        pty.key("\r")
        opened = pty.wait_for("任务", 30)
        tasks_frame = pty.screen.text()
        report.dump("real-tasks", pty)
        summary["child_observed"] = "子 Agent" in tasks_frame
        check(report, "/tasks opens during the real turn", opened, tasks_frame[-900:])
        check(
            report,
            "the real task state is live",
            "当前会话" in tasks_frame and ("运行中" in tasks_frame or "等待" in tasks_frame),
            tasks_frame[-900:],
        )
        # Delegation is the model's choice; record it either way.
        print("  child observed:", summary["child_observed"])
        pty.key("\x1b")
        time.sleep(0.5)

        # Doctor while the model may still be working.
        pty.write("/doctor")
        time.sleep(0.2)
        pty.key("\r")
        doctor_opened = pty.wait_for("诊断", 30)
        time.sleep(0.8)
        doctor_frame = pty.screen.text()
        report.dump("real-doctor", pty)
        summary["doctor_opened"] = doctor_opened
        check(report, "/doctor opens during the real turn", doctor_opened, doctor_frame[-900:])
        check(
            report,
            "the diagnostics page shows real runtime facts",
            "运行时连接" in doctor_frame and ("通过" in doctor_frame or "失败" in doctor_frame),
            doctor_frame,
        )
        check(
            report,
            "the real turn still owns the session after /doctor",
            "检查领域" in doctor_frame,
            doctor_frame,
        )
        pty.key("\x1b")
        time.sleep(1.0)
        if "检查领域" in pty.screen.text():
            # A lone Esc can be swallowed while the runtime is streaming; the
            # page must still be closable.
            pty.key("\x1b")
            time.sleep(0.5)
        check(
            report,
            "the doctor page closed back to the conversation",
            "检查领域" not in pty.screen.text(),
            pty.screen.text()[-600:],
        )

        # Wait for the real model to finish. A clarification would block the
        # turn; the prompt avoids one. 15 minutes is the ceiling for a small
        # multi-module task on a slow provider.
        terminal = False
        durable = None
        deadline = time.time() + 480
        while time.time() < deadline:
            view = pty.screen.text()
            if any(marker in view for marker in ("回答结束", "已取消", "已停止", "失败 ·")):
                terminal = True
                summary["terminal_source"] = "screen"
                break
            durable = durable_session_status(env.home)
            if durable == "completed":
                terminal = True
                summary["terminal_source"] = "session_db"
                break
            time.sleep(1.0)
        final_frame = pty.screen.text()
        if not terminal:
            # The deliverable may be complete while the model keeps polishing.
            # An idle composer is the product's own "no turn is running" fact;
            # record that this was observed instead of the terminal marker.
            idle = "输入消息" in final_frame and "Esc 打断" not in final_frame
            summary["terminal_marker"] = False
            summary["idle_composer"] = idle
            if idle:
                terminal = True
        report.dump("real-final", pty)
        summary["turn_terminal"] = final_frame[-2000:]
        check(report, "the real turn reached a terminal or an idle composer", terminal, final_frame[-1200:])
        summary["elapsed_secs"] = round(time.time() - started, 1)

        # Re-open the workbench and verify the finished task is not active.
        pty.write("/tasks")
        time.sleep(0.2)
        pty.key("\r")
        pty.wait_for("任务", 30)
        pty.key("\t")
        pty.key("\t")  # Active -> Recent -> All
        time.sleep(0.6)
        finished_tasks = pty.screen.text()
        report.dump("real-finished-tasks", pty)
        summary["final_tasks"] = finished_tasks[-2000:]
        check(
            report,
            "the finished task is not Running in /tasks",
            "当前会话" in finished_tasks and "运行中" not in finished_tasks,
            finished_tasks[-1200:],
        )

        # The deliverable is verified by the product's own test command, not
        # by the model's summary.
        tests = subprocess.run(
            [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-v"],
            cwd=env.repo,
            capture_output=True,
            text=True,
            timeout=120,
        )
        cli = subprocess.run(
            [sys.executable, "-m", "calc.cli"],
            cwd=env.repo,
            capture_output=True,
            text=True,
            timeout=60,
        )
        summary["tests"] = {
            "returncode": tests.returncode,
            "stdout": tests.stdout[-4000:],
            "stderr": tests.stderr[-4000:],
        }
        summary["cli"] = {
            "returncode": cli.returncode,
            "stdout": cli.stdout[-2000:],
            "stderr": cli.stderr[-2000:],
        }
        check(report, "the delivered package's tests pass", tests.returncode == 0, tests.stderr[-1200:] or tests.stdout[-1200:])
        check(report, "the delivered CLI prints the expected result", cli.returncode == 0 and "5" in cli.stdout, cli.stdout + cli.stderr)
    finally:
        pty.close()
        report.save()
        with open(os.path.join(EVIDENCE, "summary.json"), "w") as fh:
            json.dump(summary, fh, indent=2)
        print("summary:", os.path.join(EVIDENCE, "summary.json"))
    print("failures:", report.failures)
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
