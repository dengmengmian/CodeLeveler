"""Residue and jitter sampling for the pending-steer area (F12).

Drives a real PTY through the pending-steer transitions and samples the
EMULATED terminal screen repeatedly after each transition. A screen model is
required here: searching the raw ANSI byte stream proves what was painted at
some point, not what the terminal shows now.

Every sample asserts:
  - the composer is present;
  - the pending control area sits ABOVE the composer;
  - text that a transition removed does not appear in any frame;
  - a delivered instruction appears exactly once.

Usage: python3 scripts/steer_residue_acceptance.py <binary> <output-dir>
"""

import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pty_dogfood as dog  # noqa: E402

binary = str(Path(sys.argv[1]).resolve())
out = Path(sys.argv[2]).resolve()
out.mkdir(parents=True, exist_ok=True)
dog.EVIDENCE = str(out)
import os

dog.TRANSPORT = os.environ.get("LEVELER_PTY_TRANSPORT", "daemon")

PROMPT = "›"
report = dog.Report("steer-residue")


def screen_text(pt: dog.Pty) -> str:
    return pt.screen.text()


def rows_of(text: str, needle: str) -> list[int]:
    return [i for i, line in enumerate(text.split("\n")) if needle in line]


def samples(pt: dog.Pty, seconds: float = 0.9, interval: float = 0.04) -> list[str]:
    frames = []
    deadline = time.time() + seconds
    while time.time() < deadline:
        frames.append(screen_text(pt))
        time.sleep(interval)
    frames.append(screen_text(pt))
    return frames


def invariants(name, frames, *, present=(), absent=(), count_once=()):
    for needle in present:
        ok = all(needle in frame for frame in frames)
        report.check(name, f"present in every frame: {needle}", ok)
        if not ok:
            return False
    for needle in absent:
        ok = all(needle not in frame for frame in frames)
        report.check(name, f"removed from every frame: {needle}", ok)
        if not ok:
            return False
    for needle in count_once:
        counts = [frame.count(needle) for frame in frames]
        ok = all(c == 1 for c in counts)
        report.check(name, f"exactly once in every frame: {needle} ({sorted(set(counts))})", ok)
        if not ok:
            return False
    composer = all(PROMPT in frame for frame in frames)
    report.check(name, "the composer line is present in every frame", composer)
    above = True
    for needle in present:
        for frame in frames:
            needle_rows = rows_of(frame, needle)
            composer_rows = rows_of(frame, PROMPT)
            if needle_rows and composer_rows and min(needle_rows) >= max(composer_rows):
                above = False
                break
        if not above:
            break
    report.check(name, "the pending area stays above the composer", above)
    return composer and above


def submit(pt: dog.Pty, text: str):
    pt.write(text)
    time.sleep(0.12)
    pt.key("\r")


def wait_fast(pt: dog.Pty, needle: str, timeout: float = 20) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in screen_text(pt):
            return True
        if not pt.alive():
            return needle in screen_text(pt)
        time.sleep(0.02)
    return needle in screen_text(pt)


def wait(pt: dog.Pty, needle: str, timeout: float = 20) -> bool:
    if not pt.wait_for(needle, timeout=timeout):
        raise AssertionError(f"missing {needle!r}\n{screen_text(pt)}")
    return True


def click(pt: dog.Pty, caption: str):
    wait(pt, caption)
    for y, line in enumerate(pt.screen.lines()):
        x = line.find(caption)
        if x >= 0:
            pt.click(y, sum(dog.cell_width(ch) for ch in line[:x]) + 1)
            return
    raise AssertionError(f"caption disappeared: {caption}")


def steer_scenario():
    fake = dog.MockProvider()
    env = dog.Env("mock", fake, short=True)
    script = (
        "for i in $(seq 1 240); do echo output-line-$i; done; sleep 25"
    )
    fake.turns = [
        dog.tool_turn(
            "run_command",
            json.dumps({"program": "sh", "args": ["-c", script], "timeout_seconds": 60}),
        ),
        dog.text_turn("FINAL-RESIDUE-ANSWER"),
        # The closeout round, if the runtime asks for a final answer.
        dog.text_turn("FINAL-RESIDUE-ANSWER"),
    ]
    pt = dog.spawn(env.env, env.repo, 120, 44, [], binary)
    try:
        wait(pt, "输入消息", 40)
        frames = samples(pt, 0.4)
        report.check("empty", "nothing pending means no waiting area", all("等待投递" not in f for f in frames))
        report.check("empty", "the composer is present", all(PROMPT in f for f in frames))
        report.dump("empty", pt)

        submit(pt, "Run the long command and wait for it to finish.")
        wait(pt, "正在执行", 40)

        # 1. empty -> single
        submit(pt, "STEER_RESIDUE_ALPHA")
        wait(pt, "等待投递", 10)
        frames = samples(pt, 1.2)
        invariants("single", frames, present=("STEER_RESIDUE_ALPHA",), count_once=("STEER_RESIDUE_ALPHA",))
        positions = {tuple(rows_of(frame, "STEER_RESIDUE_ALPHA")) for frame in frames}
        report.check(
            "streaming",
            "the pending row keeps its position while tool output streams",
            len(positions) == 1,
            repr(positions),
        )
        report.dump("single", pt)

        # 2. single -> multiple
        submit(pt, "STEER_RESIDUE_BETA")
        wait(pt, "2 条等待投递", 10)
        frames = samples(pt)
        invariants("multiple", frames, present=("2 条等待投递", "STEER_RESIDUE_ALPHA"))
        report.dump("multiple", pt)

        # 3. multiple -> single (withdraw BETA from the detail view)
        pt.key("\x1bs")
        wait(pt, "待投递追加指令")
        pt.key("\x1b[B", "\x1b[3~")
        gone = dog.wait_until(pt, lambda text: "2 条等待投递" not in text, timeout=8)
        report.check("withdraw", "the second row left the summary", gone)
        pt.key("\x1b")
        wait(pt, "等待投递", 8)
        frames = samples(pt)
        invariants("withdraw", frames, present=("STEER_RESIDUE_ALPHA",), absent=("STEER_RESIDUE_BETA",))
        report.dump("withdrawn", pt)

        # 4. pending -> editing -> cancel
        pt.key("\x1bs")
        wait(pt, "待投递追加指令")
        pt.key("e")
        wait(pt, "正在编辑追加指令", 8)
        frames = samples(pt)
        report.check("editing", "the edit state names itself", all("正在编辑追加指令" in f for f in frames))
        report.dump("edit", pt)
        pt.key("\x1b")
        wait(pt, "等待投递", 8)
        frames = samples(pt)
        invariants("edit-cancelled", frames, present=("等待投递", "STEER_RESIDUE_ALPHA"), absent=("正在编辑追加指令",))
        report.dump("edit-cancelled", pt)

        # 5. pending -> editing -> save (replaces the text)
        pt.key("\x1bs")
        wait(pt, "待投递追加指令")
        pt.key("e")
        wait(pt, "正在编辑追加指令", 8)
        pt.key("\x15")
        pt.write("STEER_RESIDUE_GAMMA")
        time.sleep(0.2)
        pt.key("\r")
        wait(pt, "STEER_RESIDUE_GAMMA", 8)
        frames = samples(pt)
        invariants("saved", frames, present=("STEER_RESIDUE_GAMMA",), absent=("STEER_RESIDUE_ALPHA",))
        report.dump("saved", pt)

        # 6. priority -> safe wait (send-all from the detail view)
        pt.key("\x1bs")
        wait(pt, "待投递追加指令")
        pt.key("a")
        wait(pt, "等待安全投递", 8)
        pt.key("\x1b")
        frames = samples(pt)
        invariants("priority", frames, present=("等待安全投递",))
        report.dump("safe-wait", pt)

        # 7. withdrawn
        pt.key("\x1bs")
        wait(pt, "待投递追加指令")
        pt.key("\x1b[3~")
        gone = dog.wait_until(pt, lambda text: "STEER_RESIDUE_GAMMA" not in text, timeout=8)
        report.check("withdrawn", "the withdrawn text left the screen", gone)
        pt.key("\x1b")
        time.sleep(0.3)
        frames = samples(pt)
        invariants("withdrawn-final", frames, absent=("STEER_RESIDUE_GAMMA", "等待投递"))
        report.dump("withdrawn-final", pt)

        # 8. resize with a pending row, then history scrolling
        submit(pt, "STEER_RESIDUE_DELTA")
        wait(pt, "等待投递", 10)
        pt.resize(72, 30)
        frames = samples(pt)
        invariants("resized", frames, present=("STEER_RESIDUE_DELTA",))
        report.dump("resized", pt)
        pt.resize(120, 44)
        pt.wheel(True, times=6)
        time.sleep(0.4)
        frames = samples(pt, 0.5)
        invariants("scrolled", frames, present=("STEER_RESIDUE_DELTA",))
        report.dump("scrolled", pt)
        pt.wheel(False, times=8)
        time.sleep(0.4)

        # 9. delivered: one conversation row, nothing left waiting
        wait(pt, "回答结束", 60)
        frames = samples(pt, 0.6)
        invariants(
            "delivered",
            frames,
            present=("FINAL-RESIDUE-ANSWER",),
            absent=("等待投递", "等待安全投递", "正在编辑追加指令"),
            count_once=("STEER_RESIDUE_DELTA",),
        )
        report.dump("delivered", pt)
    finally:
        pt.close()


def recovery_scenario():
    fake = dog.MockProvider()
    env = dog.Env("mock", fake, short=True)
    fake.turns = [
        dog.status_turn(504, "gateway timeout"),
        dog.tool_turn(
            "update_goal",
            json.dumps({"status": "complete", "summary": "recovered"}),
        ),
        dog.text_turn("RECOVERED-ANSWER"),
    ]
    fake.stream_delay = 0.15
    pt = dog.spawn(env.env, env.repo, 120, 44, [], binary)
    try:
        wait(pt, "输入消息", 40)
        submit(pt, "/goal finish the residue check")
        seen = wait_fast(pt, "正在恢复任务", 20)
        report.check("goal-recovery", "the recovery state is shown", seen)
        if seen:
            frames = samples(pt, 0.5)
            report.check(
                "goal-recovery",
                "the recovery state is a live status, not a failure card",
                all(not any(bad in f for bad in ("失败", "错误", "⚠")) for f in frames),
                screen_text(pt)[:600],
            )
            report.dump("goal-recovery", pt)
        wait(pt, "RECOVERED-ANSWER", 40)
        frames = samples(pt, 0.6)
        report.check(
            "goal-recovery",
            "the recovery status leaves no stale glyphs",
            all("正在恢复任务" not in f for f in frames),
            screen_text(pt)[:800],
        )
        report.dump("goal-final", pt)
    finally:
        pt.close()


try:
    steer_scenario()
    recovery_scenario()
finally:
    report.save()
    print(json.dumps({"failures": report.failures, "blocked": report.blocked}))
    if report.failures:
        sys.exit(1)
