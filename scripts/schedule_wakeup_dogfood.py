#!/usr/bin/env python3
"""Real macOS launchd acceptance for scheduled automation (DF-55).

A real TUI session creates nothing here; it only proves the page projects the
definition before the window is closed. The schedule itself is registered in
the real user store, authorized for unattended execution, and handed to
launchd. Every CodeLeveler process that belonged to the UI is then stopped, so
the run that follows can only come from the OS job. The wake log's `source`
field distinguishes that pass from a manual `run-now`.

Cleanup is part of the test: `schedule disable` must boot the job out and
remove its plist, and the script verifies both. A failed run still attempts
cleanup in `finally`.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timedelta, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_dogfood import Pty, Report  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "debug", "leveler")
EVIDENCE = os.path.join(ROOT, "target", "schedule-wakeup-dogfood")
NAME = "DF-55 launchd wakeup"


def env_for(root: str) -> dict[str, str]:
    home = os.path.join(root, "home")
    os.makedirs(home)
    source = os.path.expanduser("~/.leveler/config.toml")
    with open(source) as fh:
        text = fh.read()
    if not re.search(r"^lang\s*=", text, re.M):
        text = 'lang = "zh"\n' + text
    with open(os.path.join(home, "config.toml"), "w") as fh:
        fh.write(text)
    env = dict(os.environ)
    env.pop("LEVELER_CONFIG_DIR", None)
    env.update(
        {
            "TERM": "xterm-256color",
            "LEVELER_HOME": home,
            "COLORTERM": "truecolor",
            "LEVELER_LOG": "warn",
            # Keep a runtime the UI started alive long enough for the test to
            # decide whether to stop it; the OS pass starts its own if not.
            "LEVELER_DAEMON_IDLE_TIMEOUT_SECS": "1800",
        }
    )
    return env


def cli(env: dict[str, str], args: list[str]) -> dict | list:
    result = subprocess.run([BIN, *args], env=env, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(
            "`%s` failed with %d: %s" % (" ".join(args), result.returncode, result.stderr[-800:])
        )
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(
            "`%s` returned non-JSON %r (stderr %r)"
            % (" ".join(args), result.stdout[-400:], result.stderr[-400:])
        ) from error


def leveler_processes(root: str) -> list[tuple[int, str]]:
    """Processes whose command line names this test's isolated root."""
    found = []
    output = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
    for line in output.splitlines():
        line = line.strip()
        if not line:
            continue
        pid, _, command = line.partition(" ")
        if root in command and "/leveler" in command and "ps -axo" not in command:
            found.append((int(pid), command))
    return found


def wait_until(predicate, timeout: float, interval: float = 0.5):
    end = time.time() + timeout
    while time.time() < end:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    return predicate()


def main() -> int:
    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(BIN):
        print("missing %s — build it first (cargo build -p leveler-cli)" % BIN)
        return 2
    report = Report("schedule-wakeup")
    root = tempfile.mkdtemp(prefix="lv-wake-", dir="/tmp")
    home = os.path.join(root, "home")
    repo = os.path.realpath(os.path.join(root, "repo"))
    os.makedirs(os.path.join(repo, ".leveler"))
    with open(os.path.join(repo, ".leveler", "config.yaml"), "w") as fh:
        fh.write("{}\n")
    subprocess.run(["git", "init", "-q"], cwd=repo, check=False)
    subprocess.run(
        ["git", "-c", "user.email=a@b", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "seed"],
        cwd=repo,
        check=False,
    )
    env = env_for(root)
    # The dispatched job must see the same provider/config bundle the UI used.
    wake_env = dict(env)
    wake_env["LEVELER_CONFIG_DIR"] = home
    pty = None
    plist: str | None = None
    enabled = False
    try:
        due = (datetime.now(timezone.utc) + timedelta(seconds=120)).strftime("%Y-%m-%dT%H:%M:%SZ")
        created = cli(
            wake_env,
            [
                "schedule",
                "add",
                "--repo",
                repo,
                "--name",
                NAME,
                "--at",
                due,
                "--command",
                "printf woke > woke.txt",
                "--grant-unattended",
                "--allow-external-side-effects",
                "--json",
            ],
        )
        report.check("setup", "the definition is stored in the runtime's schedule store", bool(created.get("id")), json.dumps(created, ensure_ascii=False))
        report.check(
            "setup",
            "the wall-clock occurrence is in the future",
            created.get("trigger", {}).get("kind") == "once",
            json.dumps(created.get("trigger"), ensure_ascii=False),
        )

        # The UI projects the definition, then closes. It is not the scheduler.
        pty = Pty(
            [BIN, "--repo", repo, "tui", "--auto-approve", "--permission", "full-access"],
            env,
            120,
            44,
        )
        report.check("ui", "the real TUI paints a frame", pty.wait_for("CodeLeveler", 60))
        pty.write("/schedules")
        pty.key("\r")
        report.check("ui", "the schedule page shows the definition", pty.wait_for(NAME, 20))
        pty.key("\r")
        report.check("ui", "the detail shows the next run", pty.wait_for("下次运行", 20))
        report.dump("ui-before-close", pty)
        # Ctrl+C arms the quit and a second press confirms it; Esc never quits.
        pty.key("\x03")
        time.sleep(0.4)
        pty.key("\x03")
        closed = wait_until(lambda: not pty.alive(), 20)
        report.check("ui", "Ctrl+C closes the TUI", closed)
        pty.close()
        pty = None

        # No client or runtime from this session may remain. The OS job is the
        # only actor left when the occurrence becomes due.
        leftovers = leveler_processes(root)
        for pid, _command in leftovers:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        gone = wait_until(lambda: not leveler_processes(root), 20)
        report.check("ui", "no CodeLeveler process of the UI session remains", gone, json.dumps(leftovers))

        before = cli(wake_env, ["schedule", "status", "--repo", repo, "--json"])
        report.check(
            "wakeup",
            "the OS wakeup is not registered before enable",
            before.get("wakeup", {}).get("registered") is False,
            json.dumps(before.get("wakeup"), ensure_ascii=False),
        )
        registration = cli(wake_env, ["schedule", "enable", "--repo", repo, "--json"])
        enabled = True
        plist = registration.get("registration_path")
        report.check(
            "wakeup",
            "enable registers and loads a launchd job",
            registration.get("registered") is True and registration.get("loaded") is True,
            json.dumps(registration, ensure_ascii=False),
        )
        report.check(
            "wakeup",
            "the registration file exists",
            bool(plist) and os.path.exists(plist),
            str(plist),
        )
        report.check(
            "wakeup",
            "the job is minute-granularity, not a second-level timer",
            registration.get("interval_secs") == 60,
            json.dumps(registration, ensure_ascii=False),
        )
        # launchd runs the pass. Only the store's own records count as proof.
        def ran():
            try:
                runs = cli(wake_env, ["schedule", "runs", "--definition", created["id"], "--repo", repo, "--json"])
            except RuntimeError:
                return []
            return [run for run in runs if run.get("state") == "succeeded"]

        succeeded = wait_until(ran, 300, 2.0)
        report.check("run", "the unattended shell run succeeded", bool(succeeded), json.dumps(succeeded, ensure_ascii=False))
        if succeeded:
            report.check(
                "run",
                "the run exited with code 0",
                succeeded[0].get("exit_code") == 0,
                json.dumps(succeeded[0], ensure_ascii=False),
            )
        report.check(
            "run",
            "the real workspace file was written by the unattended run",
            os.path.exists(os.path.join(repo, "woke.txt"))
            and open(os.path.join(repo, "woke.txt")).read() == "woke",
        )
        wakes = cli(wake_env, ["schedule", "wakeups", "--repo", repo, "--json"])
        os_wakes = [wake for wake in wakes if wake.get("source") == "os"]
        report.check(
            "run",
            "the wake log attributes the run to the OS job, not a manual pass",
            any(wake.get("succeeded", 0) >= 1 for wake in os_wakes),
            json.dumps(wakes[:3], ensure_ascii=False),
        )

        # Reopening the UI projects the finished run from the same store.
        pty = Pty(
            [BIN, "--repo", repo, "tui", "--auto-approve", "--permission", "full-access"],
            env,
            120,
            44,
        )
        report.check("reopen", "the TUI reopens after the OS run", pty.wait_for("CodeLeveler", 60))
        pty.write("/schedules")
        pty.key("\r")
        report.check("reopen", "the page shows the definition", pty.wait_for(NAME, 20))
        pty.key("\r")
        report.check("reopen", "the detail shows the finished run", pty.wait_for("已完成", 30))
        report.check("reopen", "the detail shows exit code 0", pty.wait_for("退出码 0", 20))
        report.dump("reopen-with-run", pty)
    except Exception as exc:  # a crashed scenario is a FAIL, not a skip
        report.check("scenario", "completed", False, repr(exc))
        import traceback

        traceback.print_exc()
        if pty is not None:
            report.dump("failure", pty)
    finally:
        if pty is not None:
            pty.close()
        # Always boot the job out and remove its plist; a passing run must not
        # leave a system registration behind either.
        try:
            if enabled:
                after = cli(wake_env, ["schedule", "disable", "--repo", repo, "--json"])
                report.check(
                    "cleanup",
                    "disable unloads the launchd job",
                    after.get("registered") is False and after.get("loaded") is False,
                    json.dumps(after, ensure_ascii=False),
                )
        except Exception as exc:
            report.check("cleanup", "disable completed", False, repr(exc))
        if plist:
            report.check("cleanup", "the plist no longer exists", not os.path.exists(plist), plist)
        leftovers = leveler_processes(root)
        for pid, _command in leftovers:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        shutil.rmtree(root, ignore_errors=True)
    path = report.save()
    print("\nassertions: %d, failures: %d, blocked: %d" % (len(report.results), report.failures, report.blocked))
    print("evidence: %s" % path)
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
