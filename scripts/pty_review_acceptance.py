#!/usr/bin/env python3
"""Real-PTY acceptance for manual `/review`.

Drives the real `leveler` binary through a real pseudo-terminal with the
machine's own provider configuration. The review target is this working tree's
new review implementation; the reviewer is a real model.

    LEVELER_BIN=target/debug/leveler python3 scripts/pty_review_acceptance.py

Exit 0 means: the card appeared and settled, the report page opened from the
card, and the review did not modify the working tree.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pty_dogfood as dog  # noqa: E402

ROOT = dog.ROOT
BIN = os.environ.get("LEVELER_BIN", os.path.join(ROOT, "target/debug/leveler"))
EVIDENCE = os.path.join(ROOT, "target", "review-pty")
REVIEW_TARGET = "crates/leveler-app/src/interactive/review.rs"
FOCUS = "检查这次手动审查实现的核心正确性、权限边界和状态持久化"


def isolated_env() -> tuple[str, dict[str, str]]:
    root = tempfile.mkdtemp(prefix="leveler-review-")
    home = os.path.join(root, "home")
    os.makedirs(home)
    shutil.copyfile(os.path.expanduser("~/.leveler/config.toml"), os.path.join(home, "config.toml"))
    env = dict(os.environ)
    env.update(
        {
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "LEVELER_HOME": home,
            "LEVELER_LOG": "warn",
        }
    )
    return root, env


def git_status() -> str:
    return subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True)


def wait_settled(pty: dog.Pty, timeout: float) -> str:
    deadline = time.time() + timeout
    settled = ("审查完成", "审查未完成", "审查已取消", "审查失败")
    while time.time() < deadline:
        text = pty.screen.text()
        if any(marker in text for marker in settled):
            return text
        time.sleep(1.0)
    raise TimeoutError(pty.screen.text()[-4000:])


def main() -> int:
    os.makedirs(EVIDENCE, exist_ok=True)
    before = git_status()
    root, env = isolated_env()
    pty = dog.Pty(
        [
            BIN,
            "--repo",
            ROOT,
            "tui",
            "--auto-approve",
            "--in-process",
            "--permission",
            "full-access",
        ],
        env,
        140,
        46,
    )
    try:
        if not (pty.wait_for("leveler", timeout=60) or pty.wait_stable(1.0, 60)):
            raise RuntimeError("the TUI never painted a frame")
        with open(os.path.join(EVIDENCE, "boot.txt"), "w") as fh:
            fh.write(pty.screen.text())

        pty.write(f"/review @path {REVIEW_TARGET} {FOCUS}")
        time.sleep(0.3)
        pty.key("\r")

        # Admission: the card must appear before the model finishes.
        pty.wait_for("审查中", timeout=60)
        with open(os.path.join(EVIDENCE, "started.txt"), "w") as fh:
            fh.write(pty.screen.text())

        settled = wait_settled(pty, timeout=1500)
        with open(os.path.join(EVIDENCE, "settled.txt"), "w") as fh:
            fh.write(settled)

        pos = pty.screen.find("查看报告")
        if pos is None:
            raise RuntimeError("settled card has no report entry:\n" + settled)
        pty.click(*pos)
        pty.wait_for("审查报告", timeout=30)
        with open(os.path.join(EVIDENCE, "report.txt"), "w") as fh:
            fh.write(pty.screen.text())
        for page in range(1, 6):
            pty.key("\x1b[6~")  # PageDown; the report can span several pages.
            time.sleep(0.4)
            with open(os.path.join(EVIDENCE, "report-page%d.txt" % page), "w") as fh:
                fh.write(pty.screen.text())

        after = git_status()
        if after != before:
            with open(os.path.join(EVIDENCE, "workspace-before.txt"), "w") as fh:
                fh.write(before)
            with open(os.path.join(EVIDENCE, "workspace-after.txt"), "w") as fh:
                fh.write(after)
            raise RuntimeError("the review changed the working tree")

        summary = {
            "ok": True,
            "target": REVIEW_TARGET,
            "focus": FOCUS,
            "settled_markers": [
                marker for marker in ("审查完成", "审查未完成", "审查已取消", "审查失败") if marker in settled
            ],
            "report_entry": pos,
            "workspace_unchanged": True,
            "evidence": EVIDENCE,
        }
        with open(os.path.join(EVIDENCE, "result.json"), "w") as fh:
            json.dump(summary, fh, ensure_ascii=False, indent=2)
        print(json.dumps(summary, ensure_ascii=False, indent=2))
        return 0
    finally:
        pty.close()
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - evidence-first acceptance script
        os.makedirs(EVIDENCE, exist_ok=True)
        with open(os.path.join(EVIDENCE, "failure.txt"), "w") as fh:
            fh.write(repr(error))
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
