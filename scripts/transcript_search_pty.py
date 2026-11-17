#!/usr/bin/env python3
"""Real-PTY acceptance for `/find` and `/jump`.

Drives the real `leveler` binary on a real pseudo-terminal with the repo's
scripted OpenAI-wire provider. The tool call is real: `shell_command` runs
`seq`, the runtime externalizes the >budget result into an artifact, and the
TUI searches that artifact from disk. Nothing here is a TestBackend render.

Usage:
    cargo build -p leveler-cli --bin leveler
    python3 scripts/transcript_search_pty.py

Evidence (screen dumps + report) goes to target/transcript-search-pty/.
Exit 0 = every assertion held; 2 = blocked (precondition unavailable);
1 = a real failure.
"""

from __future__ import annotations

import glob
import json
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pty_dogfood as base  # noqa: E402

NEEDLE = "TRANSCRIPT_SEARCH_NEEDLE"
CN = "退款审批"
ANSWER = "TRANSCRIPT_SEARCH_ANSWER"
PROMPT_ONE = "run the refund suite"
PROMPT_TWO = "summarize the refund verification"
ARTIFACT_LINES = 200_000

REPORT = base.Report("transcript-search")
base.EVIDENCE = os.path.join(base.ROOT, "target", "transcript-search-pty")


def streamed_text_turn(text: str, pieces: int = 40) -> list[str]:
    """One answer delivered as many small deltas, so it streams long enough
    to open `/find` while the turn is still running."""
    frames = []
    step = max(1, len(text) // pieces)
    for at in range(0, len(text), step):
        frames.append(
            json.dumps({"choices": [{"delta": {"content": text[at : at + step]}}]})
        )
    frames.append(json.dumps({"choices": [{"delta": {}, "finish_reason": "stop"}]}))
    return frames


def script():
    command = "seq 1 %d; printf '%s %s\\n'" % (ARTIFACT_LINES, CN, NEEDLE)
    answer = (
        "Refund workflow verification completed. "
        + ("checked the idempotent refund path. " * 12)
        + "\n\n"
        + ANSWER
    )
    return [
        base.tool_turn("shell_command", json.dumps({"cmd": command})),
        streamed_text_turn(answer),
        base.text_turn("Summary: the refund workflow is verified."),
    ]


def flat(pty) -> str:
    return base.flat(pty)


def wait(pty, predicate, timeout=60.0) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        if predicate():
            return True
        if not pty.alive():
            return predicate()
        time.sleep(0.1)
    return predicate()


def to_conversation(pty):
    """Leave any page the previous step opened. Esc is the page's own back;
    on the conversation it does nothing to the draft."""
    pty.key("\x1b")
    time.sleep(0.45)


def command(pty, text, settle=0.6):
    to_conversation(pty)
    pty.write(text)
    time.sleep(0.25)
    pty.key("\r")
    time.sleep(settle)


def check(name, ok, detail=""):
    REPORT.check("transcript-search", name, ok, detail)


def artifact_files(home: str) -> list[str]:
    patterns = [
        os.path.join(home, "**", "artifacts", "*.txt"),
    ]
    found: list[str] = []
    for pattern in patterns:
        found.extend(glob.glob(pattern, recursive=True))
    return [path for path in found if os.path.isfile(path)]


def session_id(env, repo: str) -> str | None:
    result = subprocess.run(
        [base.BIN, "--repo", repo, "sessions", "list"],
        cwd=repo,
        env=env.env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    match = re.search(
        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        result.stdout,
    )
    return match.group(0) if match else None


def open_find(pty, query: str, timeout=30.0):
    command(pty, "/find " + query, settle=0.3)
    return wait(pty, lambda: flat(pty).find("会话查找") >= 0, timeout)


def search_has_results(pty, token: str, timeout=60.0) -> bool:
    return wait(
        pty,
        lambda: token in pty.screen.text() and "输出" in flat(pty),
        timeout,
    )


def main() -> int:
    os.makedirs(base.EVIDENCE, exist_ok=True)
    if not os.path.exists(base.BIN):
        print("missing %s — build it first" % base.BIN)
        return 2
    mock = base.MockProvider()
    mock.turns = script()
    mock.assist_text = "noop"
    mock.stream_delay = 0.05
    env = base.Env("mock", mock)
    pty = None
    try:
        pty = base.boot(REPORT, env, 120, 44)
        check("boot", True)

        # Open the search WHILE the real turn is still running. The answer is
        # streamed in small deltas so the window is deterministic.
        pty.write(PROMPT_ONE)
        time.sleep(0.2)
        pty.key("\r")
        time.sleep(0.5)
        answer_not_yet = ANSWER not in pty.screen.text()
        # No `to_conversation` here: on a busy conversation Esc cancels the
        # turn, which is exactly what this check must NOT do.
        pty.write("/find refund")
        time.sleep(0.25)
        pty.key("\r")
        time.sleep(0.5)
        page_while_busy = "会话查找" in flat(pty)
        check(
            "/find opens during a running turn without cancelling it",
            answer_not_yet and page_while_busy,
            pty.screen.text()[:800],
        )
        REPORT.dump("00-find-while-busy", pty)
        pty.key("\x1b")  # back to the conversation; the turn keeps running
        time.sleep(0.3)
        answered = wait(pty, lambda: ANSWER in pty.screen.text(), 180)
        # Wait for the turn terminal before any later Esc: Esc on a still-busy
        # conversation is the cancel binding, and the harness must not turn its
        # own key into "the search disturbed the turn".
        terminal = wait(
            pty,
            lambda: any(
                word in flat(pty)
                for word in ("回答结束", "任务已完成", "已取消", "失败", "未完成")
            ),
            60,
        )
        not_cancelled = "已取消" not in flat(pty)
        check(
            "a scripted real turn completed normally",
            answered and terminal and not_cancelled,
            pty.screen.text()[:500],
        )
        REPORT.dump("01-turn-complete", pty)
        if not answered:
            return 1

        artifacts = artifact_files(env.home)
        check(
            "the large tool result became a real artifact",
            bool(artifacts),
            str(artifacts),
        )

        # --- Search: user text, assistant answer, tool arguments, artifact.
        opened = open_find(pty, NEEDLE)
        check("/find opens on a real PTY", opened, pty.screen.text()[:500])
        check(
            "the artifact match is reported (source 输出)",
            search_has_results(pty, NEEDLE),
            pty.screen.text()[:800],
        )
        REPORT.dump("02-find-artifact-result", pty)

        command(pty, "/find refund suite", settle=0.4)
        check(
            "a user-message match is found",
            wait(pty, lambda: "refund" in pty.screen.text().lower() and "用户" in flat(pty)),
            pty.screen.text()[:500],
        )
        REPORT.dump("03-find-user-result", pty)

        command(pty, "/find " + ANSWER, settle=0.4)
        check(
            "an assistant-answer match is found",
            wait(pty, lambda: ANSWER in pty.screen.text() and "助手" in flat(pty)),
            pty.screen.text()[:500],
        )

        # --- Chinese search.
        command(pty, "/find " + CN, settle=0.4)
        check(
            "a Chinese query finds the Chinese text",
            search_has_results(pty, CN),
            pty.screen.text()[:500],
        )
        REPORT.dump("04-find-chinese", pty)

        # --- Open the full artifact at the matched line.
        command(pty, "/find " + NEEDLE, settle=0.3)
        searched = search_has_results(pty, NEEDLE)
        # The needle is also in the call's arguments, which sort first. Move
        # to the output row: that is the one with a line in the artifact.
        pty.key("\x1b[B")
        time.sleep(0.3)
        pty.key("\r")
        time.sleep(1.2)
        in_output = wait(pty, lambda: "完整输出" in flat(pty), 20)
        check(
            "Enter opens the full output page for the artifact",
            searched and in_output,
            pty.screen.text()[:800],
        )
        REPORT.dump("05-open-artifact", pty)

        # --- Turns: two user turns, direct jump.
        pty.key("\x1b")  # leave the output page if it is still open
        time.sleep(0.5)
        base.ask(pty, PROMPT_TWO)
        wait(pty, lambda: "Summary: the refund workflow is verified." in pty.screen.text(), 120)

        command(pty, "/jump", settle=0.8)
        turns_page = flat(pty)
        check(
            "/jump opens the Turns list",
            "第1回合" in turns_page and "第2回合" in turns_page,
            pty.screen.text()[:800],
        )
        REPORT.dump("06-turns", pty)
        pty.key("\x1b[B")  # Down
        time.sleep(0.2)
        pty.key("\r")
        time.sleep(1.0)
        check(
            "Enter on a turn returns to that turn",
            "summarize the refund verification" in pty.screen.text(),
            pty.screen.text()[:800],
        )
        REPORT.dump("07-turn-two", pty)

        command(pty, "/jump 1", settle=1.0)
        check(
            "/jump 1 lands on the first turn",
            "run the refund suite" in pty.screen.text(),
            pty.screen.text()[:800],
        )

        # --- No results and the scope line.
        command(pty, "/find ZZ_NOT_IN_THIS_SESSION", settle=0.8)
        check(
            "an absent query says there is no match",
            wait(pty, lambda: "没有匹配内容" in flat(pty)),
            pty.screen.text()[:500],
        )
        check(
            "the page states its real scope",
            "范围" in flat(pty),
            flat(pty)[:500],
        )
        REPORT.dump("08-no-results", pty)

        # --- Resize while the page is open.
        command(pty, "/find " + NEEDLE, settle=0.3)
        wait(pty, lambda: NEEDLE in pty.screen.text(), 20)
        pty.resize(60, 20)
        time.sleep(0.5)
        small = pty.alive() and "会话查找" in flat(pty)
        pty.resize(120, 44)
        time.sleep(0.5)
        restored = pty.alive() and "会话查找" in flat(pty)
        check("a 60x20 resize keeps the page alive", small, pty.screen.text()[:500])
        check("returning to 120x44 keeps the page alive", restored, pty.screen.text()[:500])
        REPORT.dump("09-resized", pty)

        # --- Resume: same session, search must still work.
        pty.key("\x1b")  # back to the conversation
        time.sleep(0.3)
        pty.key("\x03", "\x03")
        time.sleep(2.0)
        pty.close()
        pty = None

        sid = session_id(env, env.repo)
        check("the session id is discoverable for resume", bool(sid), str(sid))
        if sid:
            pty = base.spawn(
                env.env,
                env.repo,
                120,
                44,
                ["--session", sid],
            )
            booted = pty.wait_for("leveler", 60) or pty.wait_stable(1.0, 60)
            check("the resumed session boots", booted, pty.screen.text()[:400])
            opened = open_find(pty, NEEDLE)
            found = opened and search_has_results(pty, NEEDLE)
            check(
                "a resumed session can search its replayed history and artifact",
                found,
                pty.screen.text()[:800],
            )
            REPORT.dump("10-resumed-find", pty)

            # --- Artifact missing at jump: state it, never fake a result.
            command(pty, "/find " + NEEDLE, settle=0.3)
            searched = search_has_results(pty, NEEDLE)
            for path in artifact_files(env.home):
                try:
                    os.remove(path)
                except OSError:
                    pass
            pty.key("\x1b[B")
            time.sleep(0.3)
            pty.key("\r")
            time.sleep(1.0)
            check(
                "a swept artifact is reported, not opened as empty",
                searched and "完整输出已不可用" in flat(pty),
                pty.screen.text()[:800],
            )
            REPORT.dump("11-artifact-missing", pty)
            pty.key("\x1b")
            time.sleep(0.3)
    finally:
        if pty is not None:
            pty.close()
        mock.stop()
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
