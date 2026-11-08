#!/usr/bin/env python3
"""Phase 2C-R4 real-PTY evidence: mid-turn steer and 「※ 回顾」 stability.

This drives the REAL `leveler` binary on a REAL pseudo-terminal through the
real TUI, reducer, renderer and runtime. Frames are decoded by the VT-100
model in `pty_dogfood.py`, so every assertion is about bytes a terminal
emulator actually received, not a TestBackend.

Two layers, never conflated:

  --provider mock   a scripted OpenAI-wire server. The runtime, journal,
                    steering queue, TUI and terminal are all real; only the
                    model frames are scripted, which is what makes the
                    steer-delivery ordering deterministic.
  --provider real   the machine's own provider config, copied into an
                    isolated LEVELER_HOME (live model layer).

Usage:
    LEVELER_BIN=<bin> python3 scripts/pty_r4_steer_review.py --provider mock
    LEVELER_BIN=<bin> python3 scripts/pty_r4_steer_review.py --provider real

Evidence (frames, report, raw terminal bytes) is written under
`target/r4-pty/`. Exit 0 means every assertion held.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pty_dogfood as dog  # noqa: E402

EVIDENCE = os.path.join(dog.ROOT, "target", "r4-pty")

RECAP = "※ 回顾"

# A separate, explicit marker so a recap row is never confused with the answer.
FINAL_ANSWER = "R4-FINAL-ANSWER"
RECAP_TEXT = "R4-RECAP-完成了配置梳理，下一步核对交付顺序。"


class FrameLog:
    """Two views of the same terminal, for different questions.

    * `chunk_frames` records the screen after every byte chunk the PTY
      delivered. That is the RAW evidence: a whole-frame repaint can be caught
      mid-write, which is why a single 13 ms gap there is a paint artifact, not
      a state change.
    * `samples` records the settled screen every 100 ms. That is the cadence a
      person perceives; a row that is gone for a whole sample would be a real
      disappearance.

    Both are written to the evidence file.
    """

    SAMPLE_SECONDS = 0.1

    def __init__(self, pty_: dog.Pty, needle: str = RECAP):
        self.pty = pty_
        self.needle = needle
        self.samples: list[dict] = []
        self.chunk_frames: list[dict] = []
        self._original = pty_.screen.feed

        def feed(text: str):
            self._original(text)
            screen = pty_.screen.text()
            self.chunk_frames.append(
                {
                    "t": round(time.time(), 3),
                    "present": needle in screen,
                    "count": screen.count(needle),
                }
            )

        pty_.screen.feed = feed

    def observe(self, seconds: float) -> None:
        """Sample the settled screen at the user-visible cadence."""
        end = time.time() + seconds
        while time.time() < end:
            screen = self.pty.screen.text()
            self.samples.append(
                {
                    "t": round(time.time(), 3),
                    "present": self.needle in screen,
                    "count": screen.count(self.needle),
                }
            )
            time.sleep(self.SAMPLE_SECONDS)

    def probe(self) -> None:
        screen = self.pty.screen.text()
        self.samples.append(
            {
                "t": round(time.time(), 3),
                "present": self.needle in screen,
                "count": screen.count(self.needle),
            }
        )

    def after(self, t0: float) -> list[dict]:
        return [s for s in self.samples if s["t"] >= t0]

    def save(self, name: str):
        path = os.path.join(EVIDENCE, name + ".frames.json")
        with open(path, "w") as fh:
            json.dump(
                {
                    "sampled_every_ms": int(self.SAMPLE_SECONDS * 1000),
                    "samples": self.samples,
                    "chunk_frames": self.chunk_frames,
                },
                fh,
            )
        return path


def observe(pty_: dog.Pty, seconds: float, stop=None) -> None:
    end = time.time() + seconds
    while time.time() < end:
        if stop is not None and stop():
            break
        time.sleep(0.05)


def observations(pty_: dog.Pty, window: float, log: FrameLog, t0: float) -> dict:
    rows = log.after(t0)
    return {
        "samples": len(rows),
        "present": sum(1 for row in rows if row["present"]),
        "disappeared": [
            row
            for before, after in zip(rows, rows[1:])
            if before["present"] and not after["present"]
        ],
        "duplicates": [row for row in rows if row["count"] > 1],
    }


def report_observations(report: dog.Report, scenario: str, label: str, obs: dict) -> None:
    report.check(
        scenario,
        "%s: the recap stays visible on every sampled frame" % label,
        obs["samples"] > 0
        and obs["present"] == obs["samples"]
        and not obs["disappeared"],
        json.dumps(obs)[:800],
    )
    report.check(
        scenario,
        "%s: exactly one recap row" % label,
        not obs["duplicates"],
        json.dumps(obs["duplicates"])[:300],
    )


TRANSPORT = "in-process"


def make_env(provider, mock):
    return dog.Env(provider, mock, short=(TRANSPORT == "daemon"))


def scenario_review_stability(report: dog.Report, mock: dog.MockProvider) -> None:
    """R1/R2/R7/R8: one live turn, then recap continuity across resize/scroll.

    The mock script is: agent answer, then the recap. `text_turn` frames are
    distinct so a recap row can only come from the AwaySummary request."""
    scenario = "review"
    mock.turns = [dog.text_turn("配置已梳理完毕。" + FINAL_ANSWER)]
    env = make_env("mock", mock)
    pty_ = dog.boot(report, env, 120, 44)
    log = FrameLog(pty_)
    try:
        t_start = time.time()
        dog.ask(pty_, "梳理一下当前配置")
        report.check(
            scenario,
            "the final answer is visible",
            pty_.wait_for(FINAL_ANSWER, timeout=60),
            pty_.screen.text()[-800:],
        )
        appeared = pty_.wait_for(RECAP, timeout=90)
        report.dump("REVIEW-1-recap-visible", pty_)
        report.check(scenario, "the recap row appears after the answer", appeared,
                     pty_.screen.text()[-800:])
        if not appeared:
            return

        # R2: let the terminal settle, then sample a long quiet window. A
        # recap that flickers from a duplicate/late event shows up here.
        t0 = time.time()
        log.observe(8.0)
        settled = observations(pty_, 8.0, log, t0)
        report.dump("REVIEW-2-settled", pty_)
        report_observations(report, scenario, "settled live", settled)
        report.check(
            scenario,
            "the recap never disappears after it first appeared",
            not settled["disappeared"],
            json.dumps(settled["disappeared"][:3]),
        )

        # R7: a real resize relayouts, then must settle with the recap intact.
        pty_.resize(96, 30)
        t0 = time.time()
        log.observe(4.0)
        resized = observations(pty_, 4.0, log, t0)
        report.dump("REVIEW-3-resized", pty_)
        report_observations(report, scenario, "after resize 96x30", resized)
        report.check(
            scenario,
            "the answer survives the resize",
            FINAL_ANSWER in pty_.screen.text() or dog.scroll_until(pty_, FINAL_ANSWER),
            pty_.screen.text()[-600:],
        )

        # R8: scrolling away and back must not change the recap's presence in
        # the projection (the row may be off-screen; it must never be gone from
        # history and duplicated on the way back).
        pty_.wheel(up=True, row=15, col=10, times=5)
        time.sleep(1.5)
        pty_.wheel(up=False, row=15, col=10, times=8)
        t0 = time.time()
        log.observe(2.0)
        scrolled = observations(pty_, 2.0, log, t0)
        report.dump("REVIEW-4-scrolled-back", pty_)
        report.check(
            scenario,
            "coming back from scrollback leaves one recap row",
            not scrolled["duplicates"],
            json.dumps(scrolled["duplicates"])[:300],
        )
        report.check(
            scenario,
            "the recap is visible after scrolling back to the live edge",
            RECAP in pty_.screen.text(),
            pty_.screen.text()[-600:],
        )
        # T-05/T-06/R5/R12: a real runtime SIGKILL after completion. The TUI
        # must reattach, keep ONE final answer and ONE recap, and the recap
        # must not be regenerated by a new model request.
        if TRANSPORT == "daemon":
            assists_before = sum(
                1 for body in mock.assist_requests if "Write one concise" in body
            )
            before = dog.daemon_pids(env.repo)
            report.check(
                scenario,
                "a detached runtime owned the session before the kill",
                bool(before),
                repr(before),
            )
            killed_at = time.time()
            for pid in before:
                os.kill(pid, signal.SIGKILL)
            replacement: list[int] = []
            deadline = time.time() + 60
            while time.time() < deadline and not replacement:
                replacement = [pid for pid in dog.daemon_pids(env.repo) if pid not in before]
                time.sleep(0.2)
            report.check(
                scenario,
                "a replacement runtime is alive with a different pid",
                bool(replacement),
                "before %r after %r" % (before, replacement),
            )
            reattached = dog.wait_until(
                pty_, lambda _: RECAP in pty_.screen.text(), timeout=60
            )
            # The rebuild itself is a legitimate content change (the snapshot
            # replaces the transcript, then durable history re-projects it).
            # Stability is about the state AFTER that rebuild.
            time.sleep(0.5)
            print(
                "  [perf] reconnect: review visible %.0f ms after SIGKILL"
                % ((time.time() - killed_at) * 1000)
            )
            t0 = time.time()
            log.observe(6.0)
            after = observations(pty_, 6.0, log, t0)
            report.dump("REVIEW-5-reconnected", pty_)
            text = pty_.screen.text()
            report.check(
                scenario,
                "the recap is present again after the runtime restart",
                reattached,
                text[-800:],
            )
            report.check(
                scenario,
                "the final answer is NOT delivered twice after reconnect",
                text.count(FINAL_ANSWER) == 1,
                "count=%d" % text.count(FINAL_ANSWER),
            )
            report.check(
                scenario,
                "exactly one recap row after reconnect",
                text.count(RECAP) == 1,
                "count=%d" % text.count(RECAP),
            )
            report.check(
                scenario,
                "reconnect did not regenerate the recap with a new model call",
                sum(1 for body in mock.assist_requests if "Write one concise" in body)
                == assists_before,
                "recap requests before=%d after=%d"
                % (
                    assists_before,
                    sum(1 for body in mock.assist_requests if "Write one concise" in body),
                ),
            )
            report_observations(report, scenario, "after reconnect", after)

        log.save("review-frames")
    finally:
        pty_.close()


def scenario_steer_live(report: dog.Report, mock: dog.MockProvider) -> None:
    """S1/S2/S6/S12 on a real PTY: Enter during a running tool steers the
    SAME turn; the next real model request carries the instruction; the tool
    is not interrupted; the final answer reflects it."""
    scenario = "steer"
    mock.turns = [
        # Round 1: a real, slow tool call keeps the turn running while the
        # user types. Its side effect is observable.
        dog.tool_turn(
            "run_command",
            json.dumps({"program": "sh", "args": ["-c", "sleep 8; echo tool-done > r4-tool.txt"]}),
        ),
        # Round 2 must be the model seeing the steer.
        dog.text_turn("已改为 JSON 输出。" + FINAL_ANSWER),
    ]
    env = make_env("mock", mock)
    pty_ = dog.boot(report, env, 120, 44)
    steer_text = "保留现有测试，但把输出格式改为 JSON。"
    try:
        # No `wait_stable` here: the running spinner keeps repainting, so
        # "wait until quiet" would wait for the turn to END. Submit, then type
        # the follow-up immediately while the tool is still running.
        pty_.write("运行一个需要几秒的命令，完成后汇报。")
        time.sleep(0.2)
        pty_.key("\r")
        time.sleep(0.4)
        # The real keyboard path: type into the normal composer and press
        # Enter while the turn is still running (the tool sleeps 8 s, so the
        # turn cannot have settled yet).
        busy_before = "回答结束" not in pty_.screen.text()
        report.check(
            scenario,
            "the turn is still running when the steer is typed",
            busy_before,
            pty_.screen.text()[-600:],
        )
        pty_.write(steer_text)
        time.sleep(0.2)
        pty_.key("\r")
        # The optimistic "发送中…" can be replaced by "已保存 · 等待投递" in a
        # few milliseconds once the runtime admits the durable row. Either is
        # the product's clear acknowledgement that Enter reached THIS turn.
        sending = pty_.wait_for("发送中", timeout=1.5) or pty_.wait_for("已保存", timeout=5)
        report.dump("STEER-1-submitted", pty_)
        report.check(
            scenario,
            "Enter while the turn runs is acknowledged as input for this turn",
            sending,
            pty_.screen.text()[-800:],
        )
        delivered = pty_.wait_for("已保存", timeout=60)
        report.dump("STEER-2-delivered", pty_)
        report.check(
            scenario,
            "the steer settles as delivered, not as a new submission",
            delivered,
            pty_.screen.text()[-800:],
        )

        done = pty_.wait_for(FINAL_ANSWER, timeout=120)
        report.dump("STEER-3-final", pty_)
        report.check(scenario, "the turn reaches its final answer", done,
                     pty_.screen.text()[-800:])

        # The proof that the steer reached the MODEL, not just the transcript:
        # a later real request body carries the exact text.
        bodies = list(mock.requests)
        agent_bodies = []
        for body in bodies:
            try:
                req = json.loads(body)
            except Exception:
                continue
            if req.get("tools"):
                agent_bodies.append(req)
        delivered = any(steer_text in json.dumps(req, ensure_ascii=False) for req in agent_bodies)
        report.check(
            scenario,
            "the next real model request carries the steer text",
            delivered,
            "agent requests: %d" % len(agent_bodies),
        )
        receipts = pty_.screen.text().count("回答结束")
        report.check(
            scenario,
            "the steer joined the running turn instead of starting a second one",
            receipts == 1 and "2 轮" in pty_.screen.text(),
            "turn receipts=%d screen=%s" % (receipts, pty_.screen.text()[-500:]),
        )
        # The real tool ran exactly once despite the steer.
        side_effect = os.path.join(env.repo, "r4-tool.txt")
        report.check(
            scenario,
            "the running tool was not interrupted or re-run",
            os.path.exists(side_effect)
            and open(side_effect).read().strip() == "tool-done",
            "side effect file: %r" % os.path.exists(side_effect),
        )
        report.check(
            scenario,
            "the final answer reflects the steer semantics",
            FINAL_ANSWER in pty_.screen.text()
            and dog.scroll_until(pty_, steer_text),
            pty_.screen.text()[-800:],
        )
        # The follow-up recap belongs to the turn that just ended.
        report.check(
            scenario,
            "the post-steer turn still produces its recap",
            pty_.wait_for(RECAP, timeout=60),
            pty_.screen.text()[-800:],
        )
        log = FrameLog(pty_)
        t0 = time.time()
        log.observe(4.0)
        settled = observations(pty_, 4.0, log, t0)
        report.dump("STEER-4-settled", pty_)
        report_observations(report, scenario, "post-steer recap", settled)
        log.save("steer-frames")
        with open(os.path.join(EVIDENCE, "steer-model-requests.json"), "w") as fh:
            json.dump(
                [
                    {
                        "index": i,
                        "tools": bool(req.get("tools")),
                        "has_steer": steer_text in json.dumps(req, ensure_ascii=False),
                    }
                    for i, req in enumerate(
                        [json.loads(b) for b in bodies if b]
                    )
                ],
                fh,
                ensure_ascii=False,
                indent=2,
            )
    finally:
        pty_.close()


def scenario_steer_reconnect(report: dog.Report, mock: dog.MockProvider) -> None:
    """S10/T-10: a durable append accepted while the turn runs, the runtime
    SIGKILLed before it is consumed, and the TUI reconnecting to the same
    session. The row must appear once, stay pending exactly once, and never be
    re-submitted by the client."""
    scenario = "steer-reconnect"
    mock.turns = [
        dog.tool_turn(
            "run_command",
            json.dumps({"program": "sh", "args": ["-c", "sleep 30; echo late > r4-late.txt"]}),
        ),
        dog.text_turn("turn settled"),
    ]
    env = make_env("mock", mock)
    pty_ = dog.boot(report, env, 120, 44)
    steer_text = "R4-PENDING-STEER-保持这条补充要求"
    try:
        pty_.write("运行一个较长的前台命令，完成后汇报。")
        time.sleep(0.2)
        pty_.key("\r")
        time.sleep(0.5)
        pty_.write(steer_text)
        time.sleep(0.2)
        pty_.key("\r")
        pending_shown = pty_.wait_for("已保存", timeout=10)
        report.dump("STEER-RECONNECT-1-pending", pty_)
        report.check(
            scenario,
            "the append is accepted as pending while the tool runs",
            pending_shown,
            pty_.screen.text()[-800:],
        )
        before = dog.daemon_pids(env.repo)
        report.check(scenario, "a detached runtime owns the session", bool(before), repr(before))
        for pid in before:
            os.kill(pid, signal.SIGKILL)
        replacement: list[int] = []
        deadline = time.time() + 60
        while time.time() < deadline and not replacement:
            replacement = [pid for pid in dog.daemon_pids(env.repo) if pid not in before]
            time.sleep(0.2)
        report.check(
            scenario,
            "a replacement runtime is alive",
            bool(replacement),
            "before %r after %r" % (before, replacement),
        )
        # Reconnect: the TUI re-reads the session snapshot, which projects the
        # journal-only pending input.
        reattached = dog.wait_until(
            pty_, lambda _: steer_text in pty_.screen.text(), timeout=90
        )
        time.sleep(2.0)
        report.dump("STEER-RECONNECT-2-reconnected", pty_)
        text = pty_.screen.text()
        report.check(
            scenario,
            "the pending append is visible exactly once after reconnect",
            reattached and text.count(steer_text) == 1,
            "count=%d" % text.count(steer_text),
        )
        rows = sqlite_inputs(env)
        matching = [row for row in rows if row["content"] == steer_text]
        report.check(
            scenario,
            "the journal still holds exactly one pending row for the append",
            len(matching) == 1 and matching[0]["state"] == "pending",
            json.dumps(matching, ensure_ascii=False)[:600],
        )
        report.check(
            scenario,
            "reconnect did not fabricate a new turn for the append",
            "新任务" not in text,
            text[-500:],
        )
        with open(os.path.join(EVIDENCE, "steer-reconnect-inputs.json"), "w") as fh:
            json.dump(rows, fh, ensure_ascii=False, indent=2)
    finally:
        pty_.close()


def sqlite_inputs(env) -> list[dict]:
    import glob
    import sqlite3

    for db in glob.glob(os.path.join(env.home, "state/projects/*/sessions.db")):
        try:
            con = sqlite3.connect(db)
            con.row_factory = sqlite3.Row
            rows = [
                dict(row)
                for row in con.execute(
                    "select id, content, state, reason, ordinal, turn_id from session_inputs"
                )
            ]
            con.close()
            return rows
        except Exception:
            continue
    return []


def scenario_real_tui(report: dog.Report) -> None:
    """D4/D5: real DeepSeek, real PTY TUI, a mid-turn append, then a real
    runtime SIGKILL and reconnect. The review must come back as the SAME text
    (a regenerated review would be a second model sentence), the final answer
    must exist once, and a resize must not move it."""
    scenario = "real-tui"
    env = make_env("real", None)
    pty_ = dog.boot(report, env, 120, 44)
    marker = "R4-PTY-STEER"
    answer_marker = "R4-PTY-ANSWER"
    log = FrameLog(pty_)
    try:
        pty_.write(
            "Run exactly one foreground command with run_command: program \"sh\", "
            "args [\"-c\",\"sleep 12; echo r4-real > r4-real.txt\"]. "
            "Do not use background. After it returns, answer in one short paragraph "
            "that includes the literal token %s." % answer_marker
        )
        time.sleep(0.2)
        pty_.key("\r")
        report.check(
            scenario,
            "the real turn is running when the steer is typed",
            dog.wait_until(
                pty_,
                lambda _: "回答结束" not in pty_.screen.text()
                and ("等待模型" in pty_.screen.text() or "打断" in pty_.screen.text()),
                timeout=30,
            ),
            pty_.screen.text()[-700:],
        )
        steer = (
            "补充：最终回答必须额外包含标记 %s，并说明这是运行中追加的要求。" % marker
        )
        pty_.write(steer)
        time.sleep(0.2)
        pty_.key("\r")
        acknowledged = pty_.wait_for("发送中", timeout=1.5) or pty_.wait_for(
            "已保存", timeout=10
        )
        report.dump("REAL-1-steer-submitted", pty_)
        report.check(
            scenario,
            "Enter during the running real turn is acknowledged as this turn's input",
            acknowledged,
            pty_.screen.text()[-800:],
        )
        answered = dog.wait_until(
            pty_,
            lambda _: answer_marker in pty_.screen.text()
            and marker in pty_.screen.text()
            and "回答结束" in pty_.screen.text(),
            timeout=360,
        )
        report.dump("REAL-2-answered", pty_)
        report.check(
            scenario,
            "the real model's final answer reflects the mid-turn append",
            answered,
            pty_.screen.text()[-900:],
        )
        if not answered:
            return
        report.check(
            scenario,
            "the final answer is displayed once",
            answer_occurrences(pty_, answer_marker) == 2,
            "before-receipt count=%d (prompt echo + one answer)"
            % answer_occurrences(pty_, answer_marker),
        )
        report.check(
            scenario,
            "the tool's real side effect exists",
            os.path.exists(os.path.join(env.repo, "r4-real.txt")),
            "r4-real.txt present=%s" % os.path.exists(
                os.path.join(env.repo, "r4-real.txt")
            ),
        )

        recap_seen = pty_.wait_for(RECAP, timeout=180)
        report.dump("REAL-3-recap", pty_)
        report.check(scenario, "the real turn gets a review row", recap_seen,
                     pty_.screen.text()[-800:])
        if not recap_seen:
            return
        recap_before = recap_line(pty_)
        report.check(
            scenario,
            "exactly one review row after the real turn",
            pty_.screen.text().count(RECAP) == 1,
            "recap=%r" % recap_before,
        )

        # Live stability, then resize stability.
        t0 = time.time()
        log.observe(5.0)
        live = observations(pty_, 5.0, log, t0)
        report_observations(report, scenario, "live real review", live)
        pty_.resize(96, 30)
        time.sleep(1.0)
        t0 = time.time()
        log.observe(3.0)
        resized = observations(pty_, 3.0, log, t0)
        report.dump("REAL-4-resized", pty_)
        report_observations(report, scenario, "real review after resize", resized)
        # Compare the review at the SAME terminal width before and after the
        # reconnect; a wrap change from the resize is layout, not content.
        recap_before = recap_line(pty_)

        if TRANSPORT == "daemon":
            time.sleep(1.0)
            before = dog.daemon_pids(env.repo)
            report.check(
                scenario,
                "a detached runtime owns the real session",
                bool(before),
                repr(before),
            )
            for pid in before:
                os.kill(pid, signal.SIGKILL)
            replacement: list[int] = []
            deadline = time.time() + 90
            while time.time() < deadline and not replacement:
                replacement = [pid for pid in dog.daemon_pids(env.repo) if pid not in before]
                time.sleep(0.2)
            report.check(
                scenario,
                "a replacement runtime is alive after the real kill",
                bool(replacement),
                "before %r after %r" % (before, replacement),
            )
            reattached = dog.wait_until(
                pty_, lambda _: RECAP in pty_.screen.text(), timeout=120
            )
            time.sleep(1.0)
            report.dump("REAL-5-reconnected", pty_)
            text = pty_.screen.text()
            recap_after = recap_line(pty_)
            report.check(
                scenario,
                "the review is present again after reconnect",
                reattached,
                text[-900:],
            )
            report.check(
                scenario,
                "reconnect keeps the SAME review text (no regeneration)",
                recap_before == recap_after and recap_after != "",
                "before=%r after=%r" % (recap_before, recap_after),
            )
            # The rebuilt transcript at 30 rows may start with the recap in
            # view; the answer itself is up-screen. Its DURABLE delivery count
            # is the authoritative "delivered once" fact, and the text must be
            # reachable in scrollback.
            deliveries = durable_delivery_count(env)
            report.check(
                scenario,
                "the real final answer was delivered exactly once (durable)",
                deliveries == 1,
                "delivery_committed rows=%d" % deliveries,
            )
            report.check(
                scenario,
                "the real final answer is reachable in scrollback after reconnect",
                dog.scroll_until(pty_, answer_marker),
                pty_.screen.text()[-600:],
            )
            report.check(
                scenario,
                "exactly one review row after reconnect",
                text.count(RECAP) == 1,
                "count=%d" % text.count(RECAP),
            )
        log.save("real-tui-frames")
    finally:
        pty_.close()


def durable_delivery_count(env) -> int:
    """`delivery_committed` rows for the isolated session database.

    The durable delivery is the runtime's own "the final answer was committed"
    fact; a reconnect that redisplayed or re-delivered the answer would show
    up here, which a screen count cannot tell apart from wrapping.
    """
    import glob
    import sqlite3

    for db in glob.glob(os.path.join(env.home, "state/projects/*/sessions.db")):
        try:
            con = sqlite3.connect(db)
            row = con.execute(
                "select count(*) from events where type = 'delivery_committed'"
            ).fetchone()
            con.close()
            return int(row[0]) if row else 0
        except Exception:
            continue
    return -1


def answer_occurrences(pty_: dog.Pty, marker: str) -> int:
    """Answer-marker occurrences BEFORE the turn receipt.

    The prompt echo contains the marker once by construction, so one deliver
    shows twice. A duplicated delivery shows three times. The recap row is
    after the receipt and never counted.
    """
    text = pty_.screen.text()
    before = text.split("回答结束")[0]
    return before.count(marker)


def recap_line(pty_: dog.Pty) -> str:
    for line in pty_.screen.lines():
        if RECAP in line:
            return line.strip()
    return ""


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", choices=["mock", "real"], default="mock")
    parser.add_argument("--transport", choices=["in-process", "daemon"], default="in-process")
    parser.add_argument("--only", default="")
    args = parser.parse_args()
    global dog, TRANSPORT
    TRANSPORT = args.transport
    dog.TRANSPORT = args.transport
    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(dog.BIN):
        print("missing %s — build it first (cargo build -p leveler-cli)" % dog.BIN)
        return 2
    report = dog.Report("r4-pty-%s-%s" % (args.provider, args.transport))
    selected = [s for s in (args.only.split(",") if args.only else ["steer", "review"]) if s]
    for name in selected:
        mock = None
        if args.provider == "mock":
            mock = dog.MockProvider()
            mock.assist_text = RECAP_TEXT
        print("=== R4 scenario %s (%s provider) ===" % (name, args.provider))
        try:
            if name == "steer":
                scenario_steer_live(report, mock)
            elif name == "review":
                scenario_review_stability(report, mock)
            elif name == "real":
                scenario_real_tui(report)
            elif name == "steer-reconnect":
                scenario_steer_reconnect(report, mock)
            else:
                raise SystemExit("unknown scenario %s" % name)
        except Exception as exc:
            report.check(name, "scenario completed", False, repr(exc))
            import traceback

            traceback.print_exc()
        finally:
            if mock is not None:
                mock.stop()
    path = report.save()
    print("\nassertions: %d, failures: %d, blocked: %d"
          % (len(report.results), report.failures, report.blocked))
    print("evidence: %s" % path)
    return 1 if report.failures else (3 if report.blocked else 0)


if __name__ == "__main__":
    sys.exit(main())
