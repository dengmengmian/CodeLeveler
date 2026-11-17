#!/usr/bin/env python3
"""Real-PTY acceptance for exit protection.

Drives the REAL `leveler` binary on a REAL pseudo-terminal against a REAL
daemon (the default product transport), with a scripted model that starts a
REAL background process and then waits on it. Every assertion is about
observable process/exit behaviour, not about a dialog's wording alone:

  no-work      an idle exit shows no dialog and leaves no work behind
  stay         Esc closes the dialog and the task keeps running
  continue     the TUI exits, the process keeps writing, `leveler background`
               can still list/stop it, and a reopened session can stop it too
  stop         "Exit and stop tasks" waits for the process to die before exit
  completion   a task that finishes while the dialog is open does not turn the
               confirmed stop into an error
  narrow       the dialog remains operable at 44x20, including a live resize
  signal       SIGHUP runs the bounded stop path and does not orphan the task

Usage:
    python3 scripts/exit_protection_pty.py [--only continue,stop]

Evidence (screens, logs) is written to target/exit-protection-pty/.
Exit code 0 means every selected check held.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import pty_dogfood as H  # noqa: E402
import bg_dogfood as BG  # noqa: E402

EVIDENCE = os.path.join(H.ROOT, "target", "exit-protection-pty")
BIN = os.environ.get("LEVELER_BIN") or os.path.join(H.ROOT, "target", "debug", "leveler")

DIALOG_TITLE = "有任务正在运行"
OPT_STOP = "退出并停止任务"
OPT_BACKGROUND = "后台继续并退出"
OPT_STAY = "留下"


def tool(name: str, arguments: dict) -> list[str]:
    return H.tool_turn(name, json.dumps(arguments))


def write_service(repo: str, prefix: str = "svc") -> tuple[str, str]:
    """A real long-running process whose progress is visible on disk.

    The name is unique per run so `pgrep -f` can only match this scenario's
    process, and the heartbeat is what proves it was still executing after the
    TUI process was gone.
    """
    name = "%s-%s.sh" % (prefix, os.urandom(3).hex())
    path = os.path.join(repo, name)
    with open(path, "w") as fh:
        fh.write(
            "#!/bin/sh\n"
            "while true; do\n"
            '  date +%s >> "$(dirname "$0")/heartbeat.log"\n'
            "  sleep 0.5\n"
            "done\n"
        )
    os.chmod(path, 0o755)
    return path, name


def write_long_script(repo: str) -> tuple[str, str]:
    return write_service(repo, "svc")


def heartbeat(repo: str) -> list[int]:
    try:
        with open(os.path.join(repo, "heartbeat.log")) as fh:
            return [int(line) for line in fh if line.strip().isdigit()]
    except FileNotFoundError:
        return []


def process_alive(name: str) -> bool:
    return (
        subprocess.run(
            ["pgrep", "-f", name],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode
        == 0
    )


def kill_tree(name: str):
    subprocess.run(
        ["pkill", "-f", name],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def stop_daemon(repo: str, home: str, name: str):
    """Stop the repository daemon and the mock provider-owned processes."""
    subprocess.run(
        ["pkill", "-TERM", "-f", repo],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    kill_tree(name)


class Scenario:
    def __init__(self, report: H.Report, name: str, cols=110, rows=42):
        self.name = name
        self.report = report
        self.mock = H.MockProvider()
        self.env = H.Env("mock", self.mock, short=True)
        self.repo = self.env.repo
        self.service, self.service_name = write_long_script(self.repo)
        self.extra_services: list[str] = []
        self.cols, self.rows = cols, rows
        self.pty = None

    def boot(self, in_process: bool = False):
        argv = [
            BIN,
            "--repo",
            self.repo,
            "tui",
            "--auto-approve",
            "--permission",
            "full-access",
        ]
        if in_process:
            argv.append("--in-process")
        self.pty = H.Pty(argv, self.env.env, self.cols, self.rows)
        ok = self.pty.wait_for("CodeLeveler", timeout=25) or self.pty.wait_stable(
            0.8, 25
        )
        self.report.check(self.name, "the real TUI boots on a PTY", ok, self.screen()[:400])
        return self.pty

    def resume(self, session_id: str):
        argv = [
            BIN,
            "--repo",
            self.repo,
            "tui",
            "--auto-approve",
            "--permission",
            "full-access",
            "--session",
            session_id,
        ]
        self.pty = H.Pty(argv, self.env.env, self.cols, self.rows)
        self.pty.wait_for("CodeLeveler", timeout=25) or self.pty.wait_stable(0.8, 25)
        return self.pty

    def screen(self) -> str:
        return self.pty.screen.text() if self.pty else ""

    def start_long_work(self):
        self.mock.turns = [
            tool(
                "run_command",
                {"program": "./" + self.service_name, "args": [], "background": True},
            ),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 300}),
        ]
        self.mock.calls = 0
        self.mock.transform = BG.resolve_ordinal_task_ids
        self.pty.write("/goal start the long service and wait for it")
        time.sleep(0.3)
        self.pty.key("\r")
        running = H.wait_until(
            self.pty,
            lambda s: self.service_name in s and "后台运行" in s,
            timeout=90,
        )
        self.report.check(
            self.name,
            "a real background process is running in the TUI",
            running,
            self.screen()[-800:],
        )
        if not running:
            self.report.dump(self.name + "-start-failed", self.pty)
        return running

    def open_dialog(self):
        self.pty.write("/exit")
        time.sleep(0.3)
        self.pty.key("\r")
        shown = H.wait_until(self.pty, lambda s: DIALOG_TITLE in s, timeout=30)
        self.report.check(
            self.name,
            "the exit dialog states the real running work",
            shown,
            self.screen()[-1200:],
        )
        if not shown:
            self.report.dump(self.name + "-dialog-failed", self.pty)
        return shown

    def close(self):
        if self.pty:
            self.pty.close()
        for name in [self.service_name, *self.extra_services]:
            stop_daemon(self.repo, self.env.home, name)
        self.mock.stop()


def focused_line(screen: str, needle: str) -> str:
    for line in screen.splitlines():
        if needle in line:
            return line
    return ""


def select(screen: str, option: str) -> bool:
    """Whether `option` is the focused row (the picker's › marker)."""
    line = focused_line(screen, option)
    return "❯" in line or "›" in line


def scenario_no_work(report: H.Report):
    sc = Scenario(report, "no-work")
    try:
        pty = sc.boot()
        pty.write("/exit")
        time.sleep(0.3)
        pty.key("\r")
        end = time.time() + 20
        while time.time() < end and pty.alive():
            time.sleep(0.05)
        report.check(
            "no-work",
            "an idle exit ends the client directly",
            not pty.alive(),
            sc.screen()[-500:],
        )
        report.check(
            "no-work",
            "no confirmation dialog was shown for no work",
            DIALOG_TITLE not in sc.screen(),
            sc.screen()[-800:],
        )
    finally:
        sc.close()


def scenario_stay(report: H.Report):
    sc = Scenario(report, "stay")
    try:
        pty = sc.boot()
        sc.start_long_work()
        before = len(heartbeat(sc.repo))
        sc.open_dialog()
        report.check(
            "stay",
            "stay is the default focus",
            select(sc.screen(), OPT_STAY),
            focused_line(sc.screen(), OPT_STAY),
        )
        pty.key("\x1b")
        closed = H.wait_until(pty, lambda s: DIALOG_TITLE not in s, timeout=10)
        report.check("stay", "Esc closes the dialog", closed, sc.screen()[-800:])
        report.check("stay", "the TUI is still alive", pty.alive(), "")
        report.check("stay", "the background process is still alive", process_alive(sc.service_name), "")
        time.sleep(1.6)
        report.check(
            "stay",
            "the task kept producing output after stay",
            len(heartbeat(sc.repo)) > before,
            f"{before} -> {len(heartbeat(sc.repo))}",
        )
    finally:
        sc.close()


def background_list(repo: str, env: dict) -> list[dict]:
    result = subprocess.run(
        [BIN, "--repo", repo, "background", "list", "--json"],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if result.returncode != 0:
        return []
    try:
        value = json.loads(result.stdout)
    except json.JSONDecodeError:
        return []
    if isinstance(value, dict):
        value = value.get("tasks", [])
    return [entry for entry in value if isinstance(entry, dict)]


def scenario_continue(report: H.Report):
    sc = Scenario(report, "continue")
    try:
        pty = sc.boot()
        sc.start_long_work()
        sc.open_dialog()
        report.check(
            "continue",
            "the real task label is listed",
            sc.service_name in sc.screen(),
            sc.screen()[-1200:],
        )
        # Stay -> background is one step up.
        pty.key("\x1b[A")
        time.sleep(0.2)
        report.check(
            "continue",
            "continue is selectable for a detachable runtime",
            select(sc.screen(), OPT_BACKGROUND),
            focused_line(sc.screen(), OPT_BACKGROUND),
        )
        before = len(heartbeat(sc.repo))
        pty.key("\r")
        end = time.time() + 30
        while time.time() < end and pty.alive():
            time.sleep(0.05)
        raw = bytes(pty.raw).decode("utf-8", "replace")
        report.check("continue", "the TUI process exits", not pty.alive(), "")
        report.check(
            "continue",
            "the TUI says work continues",
            "任务继续在后台运行" in raw,
            raw[-600:],
        )
        report.check(
            "continue",
            "the real process is still alive after the client exited",
            process_alive(sc.service_name),
            "",
        )
        time.sleep(1.6)
        report.check(
            "continue",
            "its output keeps changing with no TUI",
            len(heartbeat(sc.repo)) > before,
            f"{before} -> {len(heartbeat(sc.repo))}",
        )
        entries = background_list(sc.repo, sc.env.env)
        running = [
            entry
            for entry in entries
            if entry.get("status") == "running"
            and sc.service_name in (entry.get("program", "") + " " + " ".join(entry.get("args", [])))
        ]
        report.check(
            "continue",
            "the existing task inventory still identifies it",
            bool(running),
            json.dumps(entries, ensure_ascii=False)[:800],
        )

        # Reopen the session and stop the continued task from the product.
        session = re.search(r"Session: ([0-9a-f-]+)", raw)
        report.check("continue", "the exit hint carries the session id", bool(session), raw[-400:])
        if session:
            pty.close()
            sc.pty = None
            sc.resume(session.group(1))
            visible = H.wait_until(
                sc.pty,
                lambda s: sc.service_name in s
                or ("后台" in s and re.search(r"后台\s+[1-9]", s)),
                timeout=30,
            )
            report.check(
                "continue",
                "a reopened client shows the continued task",
                visible,
                sc.screen()[-1000:],
            )
            # /exit -> stop: the destination is real, and the reopened client
            # can end it through the normal cleanup path.
            sc.open_dialog()
            report.check(
                "continue",
                "the reopened dialog still names the real task",
                sc.service_name in sc.screen(),
                sc.screen()[-1200:],
            )
            sc.pty.key("\x1b[B")  # stay -> stop
            time.sleep(0.2)
            sc.pty.key("\r")
            end = time.time() + 40
            while time.time() < end and sc.pty.alive():
                time.sleep(0.05)
            report.check(
                "continue",
                "the reopened client exits after stopping it",
                not sc.pty.alive(),
                sc.screen()[-800:],
            )
            end = time.time() + 10
            while time.time() < end and process_alive(sc.service_name):
                time.sleep(0.05)
            report.check(
                "continue",
                "the real process is gone after the stop",
                not process_alive(sc.service_name),
                "",
            )
    finally:
        sc.close()


def scenario_stop(report: H.Report):
    sc = Scenario(report, "stop")
    try:
        pty = sc.boot()
        sc.start_long_work()
        sc.open_dialog()
        pty.key("\x1b[B")  # stay -> stop
        time.sleep(0.2)
        report.check(
            "stop",
            "stop is selectable",
            select(sc.screen(), OPT_STOP),
            focused_line(sc.screen(), OPT_STOP),
        )
        pty.key("\r")
        end = time.time() + 40
        while time.time() < end and pty.alive():
            time.sleep(0.05)
        report.check("stop", "the TUI exits after cleanup", not pty.alive(), "")
        report.check(
            "stop",
            "the real process is gone before the client is gone",
            not process_alive(sc.service_name),
            "",
        )
    finally:
        sc.close()


def scenario_completion(report: H.Report):
    """A task that ends while the dialog is open must not produce a failure."""
    sc = Scenario(report, "completion")
    try:
        sc.mock.turns = [
            tool(
                "run_command",
                {"program": "sh", "args": ["-c", "sleep 5"], "background": True},
            ),
            H.text_turn("done"),
        ]
        sc.mock.calls = 0
        sc.mock.transform = BG.resolve_ordinal_task_ids
        pty = sc.boot()
        pty.write("/goal start a short background task")
        time.sleep(0.3)
        pty.key("\r")
        H.wait_until(pty, lambda s: "sleep 5" in s, timeout=60)
        sc.open_dialog()
        # The task finishes while the user is deciding.
        time.sleep(7)
        report.check(
            "completion",
            "the dialog holds its snapshot while the task ends",
            DIALOG_TITLE in sc.screen(),
            sc.screen()[-1000:],
        )
        pty.key("\x1b[B")
        time.sleep(0.2)
        pty.key("\r")
        end = time.time() + 30
        while time.time() < end and pty.alive():
            time.sleep(0.05)
        report.check(
            "completion",
            "stop with nothing left exits cleanly",
            not pty.alive(),
            sc.screen()[-800:],
        )
        report.check(
            "completion",
            "no stop-unconfirmed warning is printed",
            "未能确认任务全部停止" not in bytes(pty.raw).decode("utf-8", "replace"),
            "",
        )
    finally:
        sc.close()


def scenario_narrow(report: H.Report):
    sc = Scenario(report, "narrow", cols=44, rows=20)
    try:
        pty = sc.boot()
        sc.start_long_work()
        shown = sc.open_dialog()
        report.check(
            "narrow",
            "the dialog is operable at 44x20",
            shown and (DIALOG_TITLE in sc.screen()) and (OPT_STAY in sc.screen()),
            sc.screen()[-1200:],
        )
        pty.resize(80, 24)
        time.sleep(1.0)
        report.check(
            "narrow",
            "a live resize keeps the decision on screen",
            DIALOG_TITLE in sc.screen() and OPT_STAY in sc.screen(),
            sc.screen()[-1200:],
        )
        report.check(
            "narrow",
            "the focused safe choice survives the resize",
            select(sc.screen(), OPT_STAY),
            focused_line(sc.screen(), OPT_STAY),
        )
    finally:
        sc.close()


def scenario_signal(report: H.Report):
    sc = Scenario(report, "signal")
    try:
        pty = sc.boot()
        sc.start_long_work()
        os.kill(pty.proc.pid, signal.SIGHUP)
        end = time.time() + 30
        while time.time() < end and pty.alive():
            time.sleep(0.05)
        report.check("signal", "SIGHUP ends the client", not pty.alive(), "")
        end = time.time() + 10
        while time.time() < end and process_alive(sc.service_name):
            time.sleep(0.05)
        report.check(
            "signal",
            "the background process is stopped, not orphaned",
            not process_alive(sc.service_name),
            "",
        )
    finally:
        sc.close()


def scenario_finished(report: H.Report):
    """After continue, the task's final result is recorded, and completed work
    no longer triggers an exit dialog."""
    sc = Scenario(report, "finished")
    try:
        sc.mock.turns = [
            tool(
                "run_command",
                {"program": "sh", "args": ["-c", "sleep 6; echo done"], "background": True},
            ),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 60}),
            H.text_turn("后台任务已经完成。"),
        ]
        sc.mock.calls = 0
        sc.mock.transform = BG.resolve_ordinal_task_ids
        pty = sc.boot()
        pty.write("/goal run a short background task and report when it finishes")
        time.sleep(0.3)
        pty.key("\r")
        H.wait_until(pty, lambda s: "sleep 6" in s, timeout=90)
        sc.open_dialog()
        pty.key("\x1b[A")  # stay -> background
        time.sleep(0.2)
        pty.key("\r")
        end = time.time() + 30
        while time.time() < end and pty.alive():
            time.sleep(0.05)
        report.check("finished", "continue exits the client", not pty.alive(), "")
        terminal = None
        deadline = time.time() + 45
        while time.time() < deadline and terminal is None:
            for entry in background_list(sc.repo, sc.env.env):
                label = entry.get("program", "") + " " + " ".join(entry.get("args", []))
                if "sleep 6" in label and entry.get("status") in ("exited", "killed", "unknown"):
                    terminal = entry.get("status")
                    break
            if terminal is None:
                time.sleep(1)
        report.check(
            "finished",
            "the continued task's final result is recorded",
            terminal == "exited",
            f"terminal={terminal}",
        )
        raw = bytes(pty.raw).decode("utf-8", "replace")
        session = re.search(r"Session: ([0-9a-f-]+)", raw)
        report.check("finished", "the exit hint carries the session id", bool(session), raw[-400:])
        if session:
            pty.close()
            sc.pty = None
            sc.resume(session.group(1))
            # The work is over; wait for the TUI to show an idle session rather
            # than racing the runtime's last event.
            H.wait_until(sc.pty, lambda s: "执行中" not in s, timeout=45)
            sc.pty.write("/exit")
            time.sleep(0.3)
            sc.pty.key("\r")
            end = time.time() + 25
            while time.time() < end and sc.pty.alive():
                time.sleep(0.05)
            report.check(
                "finished",
                "a completed task no longer triggers the exit dialog",
                not sc.pty.alive() and DIALOG_TITLE not in sc.screen(),
                sc.screen()[-1000:],
            )
    finally:
        sc.close()


def scenario_multi(report: H.Report):
    """Two real concurrent services: the dialog lists both, and stop ends both."""
    sc = Scenario(report, "multi")
    second_path, second_name = write_service(sc.repo, "svc2")
    sc.extra_services.append(second_name)
    try:
        sc.mock.turns = [
            tool(
                "run_command",
                {"program": "./" + sc.service_name, "args": [], "background": True},
            ),
            tool(
                "run_command",
                {"program": "./" + second_name, "args": [], "background": True},
            ),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 300}),
        ]
        sc.mock.calls = 0
        sc.mock.transform = BG.resolve_ordinal_task_ids
        pty = sc.boot()
        pty.write("/goal start two long services and wait")
        time.sleep(0.3)
        pty.key("\r")
        both = H.wait_until(
            pty,
            lambda s: sc.service_name in s and second_name in s and "后台运行" in s,
            timeout=90,
        )
        report.check("multi", "two real services run at once", both, sc.screen()[-900:])
        sc.open_dialog()
        report.check(
            "multi",
            "the dialog lists both services with the real count",
            "3 项任务" in sc.screen()
            and sc.service_name in sc.screen()
            and second_name in sc.screen(),
            sc.screen()[-1400:],
        )
        pty.key("\x1b[B")  # stay -> stop
        time.sleep(0.2)
        pty.key("\r")
        end = time.time() + 45
        while time.time() < end and pty.alive():
            time.sleep(0.05)
        report.check("multi", "the client exits after stopping both", not pty.alive(), "")
        end = time.time() + 10
        while time.time() < end and (
            process_alive(sc.service_name) or process_alive(second_name)
        ):
            time.sleep(0.05)
        report.check(
            "multi",
            "both real processes are gone",
            not process_alive(sc.service_name) and not process_alive(second_name),
            "",
        )
    finally:
        sc.close()


def scenario_embedded(report: H.Report):
    """An in-process runtime cannot promise continuation, so the option is
    disabled with a stated reason; stop still ends the real process."""
    sc = Scenario(report, "embedded")
    try:
        sc.mock.turns = [
            tool(
                "run_command",
                {"program": "./" + sc.service_name, "args": [], "background": True},
            ),
            tool("wait_task", {"task_id": "bg-1", "timeout_seconds": 300}),
        ]
        sc.mock.calls = 0
        sc.mock.transform = BG.resolve_ordinal_task_ids
        pty = sc.boot(in_process=True)
        pty.write("/goal start the long service and wait")
        time.sleep(0.3)
        pty.key("\r")
        running = H.wait_until(
            pty,
            lambda s: sc.service_name in s and "后台运行" in s,
            timeout=90,
        )
        report.check("embedded", "the in-process runtime starts real work", running, "")
        sc.open_dialog()
        report.check(
            "embedded",
            "continuation is visibly unavailable",
            "无法继续管理" in sc.screen(),
            sc.screen()[-1400:],
        )
        pty.key("\x1b[B")  # stay -> stop
        time.sleep(0.2)
        pty.key("\r")
        end = time.time() + 45
        while time.time() < end and pty.alive():
            time.sleep(0.05)
        report.check("embedded", "the embedded client exits", not pty.alive(), "")
        report.check(
            "embedded",
            "the real process is gone",
            not process_alive(sc.service_name),
            "",
        )
    finally:
        sc.close()


SCENARIOS = {
    "no-work": scenario_no_work,
    "stay": scenario_stay,
    "continue": scenario_continue,
    "stop": scenario_stop,
    "completion": scenario_completion,
    "narrow": scenario_narrow,
    "signal": scenario_signal,
    "finished": scenario_finished,
    "multi": scenario_multi,
    "embedded": scenario_embedded,
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", default="", help="comma-separated scenario names")
    args = parser.parse_args()
    selected = [name.strip() for name in args.only.split(",") if name.strip()] or list(
        SCENARIOS
    )
    os.makedirs(EVIDENCE, exist_ok=True)
    report = H.Report("exit-protection")
    for name in selected:
        print(f"\n== {name} ==")
        SCENARIOS[name](report)
    print(f"\nfailures: {report.failures}")
    with open(os.path.join(EVIDENCE, "results.json"), "w") as fh:
        json.dump(report.results, fh, ensure_ascii=False, indent=2)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
