#!/usr/bin/env python3
"""Simultaneous real TUI + Electron acceptance over one schedule store.

One isolated LEVELER_HOME is shared by a PTY-driven TUI (in-process runtime)
and a live Electron window (its own runtime). The scenario exercises the
cross-client rules that static tests cannot: a Desktop create appears in the
open TUI, a Desktop edit invalidates the grant the TUI is showing, a stale TUI
editor is refused instead of overwriting the newer definition, pause and resume
cross both directions, and a single run is visible as one record everywhere.

No model is called. The provider endpoint is a closed port; both clients only
read and mutate the runtime's schedule store.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_dogfood import Pty, Report  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "debug", "leveler")
NODE = shutil.which("node") or "node"
SEED = "DF-58 TUI 共享任务"
DESKTOP_NAME = "DF-58 Desktop 并发任务"


def cli(env: dict[str, str], args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run([BIN, *args], env=env, capture_output=True, text=True)


def cli_json(env: dict[str, str], args: list[str]) -> dict | list:
    result = cli(env, args)
    if result.returncode != 0:
        raise RuntimeError(
            "`%s` failed with %d: %s"
            % (" ".join(args), result.returncode, result.stderr[-1200:])
        )
    return json.loads(result.stdout)


class Desktop:
    def __init__(self, env: dict[str, str], workspace: str):
        self.proc = subprocess.Popen(
            [NODE, os.path.join(ROOT, "apps", "leveler-desktop", "scripts", "schedule_concurrent.mjs")],
            cwd=os.path.join(ROOT, "apps", "leveler-desktop"),
            env={**env, "LEVELER_BINARY": BIN, "LEVELER_WORKSPACE": workspace},
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        line = self._read_line(90)
        if line.strip() != "READY":
            raise RuntimeError("Desktop did not become ready: %r %s" % (line, self._stderr()))

    def _read_line(self, timeout: float) -> str:
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError("Desktop exited: %s" % self._stderr())
            line = self.proc.stdout.readline()
            if line:
                return line
            time.sleep(0.05)
        raise RuntimeError("Desktop did not answer within %.0fs: %s" % (timeout, self._stderr()))

    def _stderr(self) -> str:
        try:
            if self.proc.poll() is not None:
                return self.proc.stderr.read()[-3000:]
        except Exception:
            pass
        return ""

    def call(self, cmd: str, timeout: float = 60, **kwargs):
        self.proc.stdin.write(json.dumps({"cmd": cmd, **kwargs}) + "\n")
        self.proc.stdin.flush()
        answer = json.loads(self._read_line(timeout))
        if not answer.get("ok"):
            raise RuntimeError("Desktop %s failed: %s" % (cmd, answer))
        return answer["result"]

    def close(self):
        if self.proc.poll() is None:
            try:
                self.proc.stdin.write(json.dumps({"cmd": "close"}) + "\n")
                self.proc.stdin.flush()
                self.proc.wait(timeout=10)
            except Exception:
                self.proc.kill()


def screen_has(pty: Pty, text: str, timeout: float = 20) -> bool:
    return pty.wait_for(text, timeout) is not None


def open_detail(pty: Pty, name: str) -> bool:
    # The list cursor starts at the first row; walk it until the detail has the
    # definition we need. Selection never edits anything.
    for _ in range(8):
        pty.key("\r")
        time.sleep(1.0)
        text = pty.screen.text()
        if name in text:
            return True
        pty.key("\x1b")
        time.sleep(0.4)
        pty.key("j")
        time.sleep(0.3)
    return False


def wait_name(env: dict[str, str], repo: str, name: str, timeout: float = 30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        rows = cli_json(env, ["schedule", "list", "--repo", repo, "--json"])
        for row in rows:
            if row.get("name") == name:
                return row
        time.sleep(0.5)
    return None


def main() -> int:
    report = Report("schedule-concurrent")
    # macOS rejects Unix socket paths over SUN_LEN (104 bytes). The default
    # per-user temp dir is already long enough that <root>/repo/.leveler/run
    # cannot bind, and the Electron runtime then fails to connect. /tmp is the
    # real short path (a symlink to /private/tmp on macOS).
    root = tempfile.mkdtemp(prefix="lvc-", dir="/tmp")
    home = os.path.join(root, "home")
    repo = os.path.join(root, "repo")
    os.makedirs(home)
    os.makedirs(os.path.join(repo, ".leveler"))
    with open(os.path.join(repo, ".leveler", "config.yaml"), "w") as fh:
        fh.write("{}\n")
    with open(os.path.join(home, "config.toml"), "w") as fh:
        fh.write(
            'default_model="fixture/m"\n'
            '[providers.fixture]\n'
            'protocol="openai_chat"\n'
            'base_url="http://127.0.0.1:9"\n'
            'api_key="test-only"\n'
            '[models.m]\n'
            'provider="fixture"\n'
            'model_id="m"\n'
            "context_window=131072\n"
        )
    repo = os.path.realpath(repo)
    env = {
        **os.environ,
        "LEVELER_HOME": home,
        "LEVELER_LANG": "zh",
        "NO_COLOR": "1",
        "LEVELER_DAEMON_IDLE_TIMEOUT_SECS": "60",
    }
    env.pop("LEVELER_CONFIG_DIR", None)
    pty = None
    desktop = None
    try:
        seeded = cli_json(
            env,
            [
                "schedule", "add", "--repo", repo,
                "--name", SEED,
                "--every", "3600",
                "--command", "printf seed",
                "--workspace", repo,
                "--json",
            ],
        )
        seed_id = seeded["id"]
        cli(env, ["schedule", "grant", seed_id, "--allow-external-side-effects"])

        pty = Pty(
            [BIN, "--repo", repo, "tui", "--in-process", "--auto-approve", "--permission", "full-access"],
            env,
            150,
            50,
        )
        report.check("setup", "the real TUI starts", pty.wait_for("CodeLeveler", 60))
        pty.write("/schedules")
        pty.key("\r")
        report.check("setup", "the schedules page opens", pty.wait_for("定时任务", 20))
        report.check("setup", "the seeded definition is visible", pty.wait_for(SEED, 20))

        desktop = Desktop(env, repo)
        report.check("setup", "the real Electron window opens Automation", True)

        # Desktop create while the TUI list stays open: the page re-reads the
        # runtime on its own; no manual refresh may be required.
        created = desktop.call("create", name=DESKTOP_NAME, command="printf desktop")
        report.check(
            "refresh",
            "a Desktop create appears in the open TUI without a reload",
            pty.wait_for(DESKTOP_NAME, 30) is not None,
            pty.screen.text()[-1200:],
        )
        report.check("refresh", "the Desktop create reached the shared store", bool(created.get("id")))

        # The grant is visible in both; a Desktop action edit must invalidate
        # it in the open TUI as well as in the store.
        report.check("grant", "the TUI shows the explicit grant", open_detail(pty, SEED))
        report.check("grant", "the granted state is painted", pty.wait_for("已授权无人值守", 15) is not None)
        desktop.call("edit", name=SEED, command="printf edited-by-desktop")
        report.check(
            "grant",
            "a Desktop action edit invalidates the grant shown by the TUI",
            pty.wait_for("需要重新授权", 30) is not None,
            pty.screen.text()[-1200:],
        )
        shown = cli_json(env, ["schedule", "show", seed_id, "--repo", repo, "--json"])
        report.check(
            "grant",
            "the runtime store agrees the grant is gone",
            shown["unattended_grant"] is None
            and shown["action"]["command"] == "printf edited-by-desktop",
            json.dumps(shown, ensure_ascii=False),
        )

        # A stale TUI editor must be refused, not silently overwrite the newer
        # Desktop definition.
        pty.key("e")
        time.sleep(1.0)
        report.check("conflict", "the TUI editor opens", "编辑" in pty.screen.text() or "名称" in pty.screen.text())
        desktop.call("edit", name=SEED, command="printf newer-from-desktop")
        time.sleep(2.0)
        for _ in range(7):
            pty.key("\r")
            time.sleep(0.2)
        report.check(
            "conflict",
            "a stale TUI save is refused with the revision conflict",
            pty.wait_for("changed since it was read", 20) is not None,
            pty.screen.text()[-1600:],
        )
        shown = cli_json(env, ["schedule", "show", seed_id, "--repo", repo, "--json"])
        report.check(
            "conflict",
            "the newer Desktop definition was not overwritten",
            shown["action"]["command"] == "printf newer-from-desktop",
            json.dumps(shown["action"], ensure_ascii=False),
        )
        pty.key("\x1b")
        time.sleep(0.5)

        # Pause on one side is visible on the other, and resume returns.
        desktop.call("pause", name=SEED)
        report.check(
            "pause",
            "a Desktop pause is visible in the open TUI",
            pty.wait_for("已暂停", 30) is not None,
            pty.screen.text()[-1200:],
        )
        pty.key("p")
        time.sleep(2.0)
        rows = cli_json(env, ["schedule", "list", "--repo", repo, "--json"])
        report.check(
            "pause",
            "a TUI resume is visible to the runtime and Desktop",
            next(row for row in rows if row["id"] == seed_id)["enabled"] is True,
        )

        # One run must be one record in both projections. The action edit above
        # invalidated the grant, so the run-now needs a fresh explicit one.
        cli(env, ["schedule", "grant", seed_id, "--allow-external-side-effects"])
        clicked_at = datetime.now(timezone.utc)
        desktop.call("runNow", name=SEED, timeout=90)
        # The run-now pass also claims any earlier slot this schedule still
        # owns (creation makes the first occurrence due immediately), and the
        # run-now itself may still be dispatching when the button's wait ends.
        # Wait until the click's own slot exists and every run is terminal.
        terminal_states = {"succeeded", "failed", "blocked", "unknown", "timed_out", "cancelled"}

        def slot(run):
            return datetime.fromisoformat(run["scheduled_for"].replace("Z", "+00:00"))

        deadline = time.time() + 90
        runs = []
        while time.time() < deadline:
            runs = cli_json(
                env, ["schedule", "runs", "--definition", seed_id, "--repo", repo, "--json"]
            )
            if (
                runs
                and any(slot(run) >= clicked_at for run in runs)
                and all(run.get("state") in terminal_states for run in runs)
            ):
                break
            time.sleep(1)
        report.check(
            "history",
            "the completed run appears in the open TUI history",
            pty.wait_for("已完成", 30) is not None,
            pty.screen.text()[-1600:],
        )
        occurrences = [run.get("occurrence_id") for run in runs]
        report.check(
            "history",
            "each run is exactly one history record",
            len(occurrences) == len(set(occurrences)) and all(occurrences),
            json.dumps(runs, ensure_ascii=False),
        )
        newest = max(runs, key=lambda run: run.get("scheduled_for") or "") if runs else None
        report.check(
            "history",
            "the run-now is the newest slot and it succeeded",
            newest is not None and newest.get("state") == "succeeded",
            json.dumps(newest, ensure_ascii=False),
        )
        desktop_state = desktop.call("state")
        report.check(
            "errors",
            "the Electron window has no page errors",
            not desktop_state.get("errors"),
            json.dumps(desktop_state.get("errors"), ensure_ascii=False),
        )
    except Exception as exc:
        report.check("harness", "scenario completed without a harness error", False, repr(exc))
    finally:
        if desktop:
            desktop.close()
        if pty:
            pty.key("\x1b")
            time.sleep(0.3)
            pty.key("\x03")
            time.sleep(0.3)
            pty.key("\x03")
            time.sleep(0.5)
            pty.close()
        shutil.rmtree(root, ignore_errors=True)
    path = report.save()
    print("assertions: %d, failures: %d, blocked: %d" % (
        len(report.results), report.failures, report.blocked
    ))
    print("evidence: %s" % path)
    return 1 if report.failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
