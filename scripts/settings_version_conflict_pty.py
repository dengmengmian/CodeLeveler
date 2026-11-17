#!/usr/bin/env python3
"""Real-PTY acceptance for the `/settings` version-conflict path.

Two REAL TUI processes share one isolated workspace and session:

  A  the user's Settings page (in-process runtime)
  B  a second legal client (in-process runtime, same database)

A opens the permission row and its picker, holding the snapshot version it
observed. B then changes the same session setting through the same typed
command. A submits the old version: the runtime must reject it, must not
overwrite B's value, and must not paint a success. A then re-reads the page and
repeats the change with the fresh version; that attempt must apply.

Nothing here is a reducer mock: both processes are the real binary on a real
pseudo-terminal, and the final value is read from the storage authority.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import pty_dogfood as base  # noqa: E402

ROOT = os.path.dirname(HERE)
EVIDENCE = os.environ.get(
    "LEVELER_EVIDENCE",
    os.path.join(
        ROOT,
        ".dev",
        "audit",
        "mcp-settings-help-ux",
        "evidence",
        "pty-final-closure",
    ),
)
os.makedirs(EVIDENCE, exist_ok=True)
base.EVIDENCE = EVIDENCE


def check(report, name, ok, detail=""):
    report.check("version-conflict", name, ok, detail)


def command(pty, text, settle=0.8):
    pty.write(text)
    time.sleep(0.2)
    pty.key("\r")
    time.sleep(settle)


def screen(pty):
    return pty.screen.text()


def wait_picker(pty, timeout=15.0):
    return pty.wait_for("选择权限模式", timeout)


def wait_picker_closed(pty, timeout=15.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if "选择权限模式" not in screen(pty):
            return True
        time.sleep(0.1)
    return False


def session_id(env):
    """The one session both processes share, read from the storage CLI."""
    result = subprocess.run(
        [base.BIN, "--repo", env.repo, "sessions", "list"],
        env=env.env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    ids = re.findall(
        r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        result.stdout,
    )
    return ids[0] if ids else None


def session_mode(env, sid):
    result = subprocess.run(
        [base.BIN, "--repo", env.repo, "sessions", "show", sid],
        env=env.env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    match = re.search(r"\bmode:\s*([a-z_]+)", result.stdout)
    return match.group(1) if match else None


def leave_page(pty):
    """Clear a page filter, leave the page, and land on the conversation."""
    for _ in range(3):
        pty.key("\x1b")
        time.sleep(0.25)
    time.sleep(0.2)


def open_permission_row(pty):
    """Filter `/settings` to the permission row and open its shared picker."""
    command(pty, "/settings")
    if not pty.wait_for("设置", 20):
        return False
    pty.write("/")
    time.sleep(0.2)
    pty.write("权限档位")
    time.sleep(0.2)
    pty.key("\r")
    time.sleep(0.3)
    pty.key("\r")
    time.sleep(0.4)
    return wait_picker(pty, 15)


def main():
    report = base.Report("settings-version-conflict")
    mock = base.MockProvider()
    mock.turns = []
    mock.assist_text = ""
    env = base.make_env("mock", mock)

    a = base.boot(report, env, 120, 46)
    sid = session_id(env)
    check(report, "the storage authority exposes the session", sid is not None, sid or "")
    if not sid:
        report.save()
        return 1

    b = base.boot(report, env, 120, 40, extra=["--session", sid])
    try:
        # A observes the settings page and opens the permission picker. Its
        # snapshot version is now pinned to what it read.
        opened = open_permission_row(a)
        check(report, "A opens the permission picker from /settings", opened, screen(a)[:800])
        report.dump("conflict-A-picker-open", a)
        # current = full_access (spawn override): Up twice reaches ask.
        a.key("\x1b[A")
        time.sleep(0.2)
        a.key("\x1b[A")
        time.sleep(0.2)
        check(
            report,
            "A's stale picker offers the value it will try to write",
            "❯ 1. ask" in screen(a),
            screen(a)[:800],
        )

        # B changes the SAME setting through the SAME typed command.
        command(b, "/permission")
        check(report, "B opens the shared permission picker", wait_picker(b), screen(b)[:800])
        b.key("\x1b[A")  # full -> assisted
        time.sleep(0.2)
        b.key("\r")
        time.sleep(0.8)
        closed = wait_picker_closed(b)
        check(report, "B's picker closes on a real runtime answer", closed, screen(b)[:800])
        check(
            report,
            "B's change is the new session value",
            "· auto ·" in screen(b),
            screen(b)[:800],
        )
        before = session_mode(env, sid)
        check(report, "B's value reached storage", before == "assisted", str(before))
        report.dump("conflict-B-changed", b)

        # A submits with the version it observed BEFORE B's write.
        a.key("\r")
        time.sleep(1.0)
        text = screen(a)
        check(
            report,
            "the stale submission is rejected as a version conflict",
            "另一个客户端" in text,
            text[:900],
        )
        check(
            report,
            "the rejection is not painted as a success",
            "已保存" not in text and "已更新" not in text,
            text[:900],
        )
        report.dump("conflict-A-rejected", a)
        after = session_mode(env, sid)
        check(
            report,
            "the stale write did not overwrite the other client",
            after == "assisted",
            f"mode={after}",
        )
        check(
            report,
            "A resynced to the runtime's current value",
            "替我审批" in text,
            text[:900],
        )

        # A can re-read the page and repeat the change with the fresh version.
        leave_page(a)
        opened = open_permission_row(a)
        check(report, "A reopens the permission row after the conflict", opened, screen(a)[:800])
        report.dump("conflict-A-retry-picker", a)
        a.key("\x1b[A")  # assisted -> ask
        time.sleep(0.2)
        a.key("\r")
        time.sleep(1.0)
        text = screen(a)
        check(
            report,
            "the retry with a fresh version is accepted",
            "另一个客户端" not in text and "请求批准" in text,
            text[:900],
        )
        final = session_mode(env, sid)
        check(report, "the final effective value is A's retry", final == "request_approval", str(final))
        report.dump("conflict-A-retried", a)
    finally:
        a.close()
        b.close()
        report.save()

    print(f"report: {os.path.join(EVIDENCE, 'report-settings-version-conflict.json')}")
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
