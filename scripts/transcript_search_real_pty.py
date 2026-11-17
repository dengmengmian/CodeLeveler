#!/usr/bin/env python3
"""Real-provider smoke for `/find` on a real PTY.

One real model turn with one real tool call, then `/find` over the user prompt,
the tool row and the assistant answer. This is the compatibility layer: the
mock run proves determinism, this proves the feature survives a model the
harness did not script. It spends a small real request; run it once.

Usage:
    cargo build -p leveler-cli --bin leveler
    python3 scripts/transcript_search_real_pty.py

Evidence goes to target/transcript-search-pty/. Exit 0 = pass, 2 = blocked
(no usable provider config), 1 = failure.
"""

from __future__ import annotations

import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pty_dogfood as base  # noqa: E402

TOKEN = "FIND_REAL_TOKEN_9f3a"
ANSWER = "REAL_ANSWER_MARKER_7c2e"
PROMPT = (
    "Use the shell_command tool to run exactly: printf '" + TOKEN + "\\n'. "
    "Then reply with one short sentence and end with the exact line "
    + ANSWER
)

REPORT = base.Report("transcript-search-real")
base.EVIDENCE = os.path.join(base.ROOT, "target", "transcript-search-pty")


def flat(pty) -> str:
    return base.flat(pty)


def wait(pty, predicate, timeout=300.0) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        if predicate():
            return True
        if not pty.alive():
            return predicate()
        time.sleep(0.2)
    return predicate()


def to_conversation(pty):
    pty.key("\x1b")
    time.sleep(0.5)


def command(pty, text, settle=0.8):
    to_conversation(pty)
    pty.write(text)
    time.sleep(0.3)
    pty.key("\r")
    time.sleep(settle)


def main() -> int:
    os.makedirs(base.EVIDENCE, exist_ok=True)
    if not os.path.exists(os.path.expanduser("~/.leveler/config.toml")):
        print("BLOCKED: no ~/.leveler/config.toml for a real provider run")
        return 2
    if not os.path.exists(base.BIN):
        print("missing %s — build it first" % base.BIN)
        return 2
    env = base.Env("real", None)
    pty = None
    try:
        pty = base.boot(REPORT, env, 120, 44)
        base.ask(pty, PROMPT)
        answered = wait(pty, lambda: ANSWER in pty.screen.text(), 300)
        REPORT.check(
            "transcript-search-real",
            "a real provider turn completed in the real TUI",
            answered,
            pty.screen.text()[:600],
        )
        REPORT.dump("real-01-turn", pty)
        if not answered:
            return 1

        command(pty, "/find " + TOKEN, settle=0.4)
        found_user_or_tool = wait(
            pty,
            lambda: TOKEN in pty.screen.text()
            and ("用户" in flat(pty) or "工具" in flat(pty) or "输出" in flat(pty)),
            60,
        )
        REPORT.check(
            "transcript-search-real",
            "/find finds the real prompt / tool row",
            found_user_or_tool and "会话查找" in flat(pty),
            pty.screen.text()[:800],
        )
        REPORT.dump("real-02-find-token", pty)

        command(pty, "/find " + ANSWER, settle=0.4)
        found_answer = wait(
            pty,
            lambda: ANSWER in pty.screen.text() and "助手" in flat(pty),
            60,
        )
        REPORT.check(
            "transcript-search-real",
            "/find finds the real assistant answer",
            found_answer,
            pty.screen.text()[:800],
        )
        REPORT.dump("real-03-find-answer", pty)

        command(pty, "/jump", settle=0.8)
        turns = flat(pty)
        REPORT.check(
            "transcript-search-real",
            "/jump lists the real turn",
            "第1回合" in turns,
            pty.screen.text()[:800],
        )
        REPORT.dump("real-04-turns", pty)
        pty.key("\r")
        time.sleep(0.8)
        REPORT.check(
            "transcript-search-real",
            "opening the real turn returns to the conversation",
            "会话查找" not in flat(pty),
            pty.screen.text()[:600],
        )
    finally:
        if pty is not None:
            pty.close()
        path = REPORT.save()
        print(
            "\nassertions: %d, failures: %d, blocked: %d"
            % (len(REPORT.results), REPORT.failures, REPORT.blocked)
        )
        print("evidence: %s" % path)
    if REPORT.failures:
        return 1
    return 3 if REPORT.blocked else 0


if __name__ == "__main__":
    sys.exit(main())
