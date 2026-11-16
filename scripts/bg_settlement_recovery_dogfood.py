#!/usr/bin/env python3
"""Final candidate scenario A: a background check survives a Goal recovery.

The machine's real provider answers through the local forwarding proxy used by
`goal_fault_dogfood.py`. The model is asked to run a real check in the
background and then cite it. The proxy holds the request that follows the
launch until the check's durable terminal exists, then answers it with a
complete HTTP 504. A complete provider error cannot be retried at the request
level, so the only continuation is a Goal recovery window.

That window is the exact shape the rebuilt-settlement fix must handle: it loads
the durable execution record before the terminal is observed, and the terminal
observation then writes the execution's unknown settlement. The window must
close on that result (not on whatever version the tree later reached), and the
check must not be rerun.

Evidence is written under test-output/final-candidate/scenario-a/.
Exit 0 means every assertion held.
"""

from __future__ import annotations

import json
import os
import re
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import bg_dogfood as BG  # noqa: E402
import goal_fault_dogfood as GF  # noqa: E402
import pty_dogfood as H  # noqa: E402

OUT = Path(
    sys.argv[1] if len(sys.argv) > 1 else "test-output/final-candidate/scenario-a"
).resolve()
OUT.mkdir(parents=True, exist_ok=True)
H.EVIDENCE = str(OUT)

PROMPT = (
    "/goal 仓库根目录有 verify.sh。先用 declare_scope 登记一个 command 验收要求"
    "（目标命令 `sh verify.sh`），再用 run_command 的 background=true 在后台运行 "
    "`sh verify.sh`，等它真实结束；完成后必须把这次后台执行的 call_id 作为验收证据"
    "（verification.detail 和 requirement.evidence 都写 call_id:<那个 id>）。"
    "注意 call_id 是 run_command 这次调用的 id（形如 call_00_...），不是 task_id"
    "（形如 host-... 或 bg-...）。如果工具结果没有直接展示 call_id，引用真实运行的命令 "
    "`sh verify.sh` 即可；不要为了找 id 去翻运行时状态目录。"
    "不要重新运行这个检查，也不要伪造结果。"
)

CHECK = "#!/bin/sh\nsleep 10\necho VERIFY-OK\nexit 0\n"


def event_payloads(home: str, kind: str) -> list[dict]:
    rows = BG.query(
        home,
        "select payload from events where type=? order by sequence",
        (kind,),
    )
    out = []
    for (payload,) in rows:
        try:
            value = json.loads(payload)
        except Exception:
            continue
        out.append(value.get("payload", value))
    return out


def wait_events(home: str, kind: str, timeout: float = 180.0) -> list[dict]:
    return BG.wait_for(
        lambda: (found if (found := event_payloads(home, kind)) else None),
        timeout=timeout,
        interval=0.1,
    ) or []


def main() -> int:
    source = os.path.expanduser("~/.leveler/config.toml")
    upstream = re.search(r'base_url\s*=\s*"(https://[^"]+)"', open(source).read())
    if upstream is None:
        raise SystemExit("the machine config names no provider base_url")

    proxy = GF.Proxy(upstream.group(1))
    proxy.fault_mode = "goal"
    # The model may spend a round on recon before launching; arm the fault
    # only after the durable start exists. The request that follows the launch
    # is then held until the check has really ended, and answered with the 504.
    proxy.fault_after = 0
    proxy.fault_armed = False
    proxy.fault_gate = threading.Event()
    threading.Thread(target=proxy.serve_forever, daemon=True).start()

    env = H.Env("real", None)
    home, repo = env.home, env.repo
    with open(os.path.join(repo, "verify.sh"), "w") as fh:
        fh.write(CHECK)
    GF.patch_config(home, proxy.port)

    report = H.Report("scenario-a-bg-resume-real")
    pty_ = H.boot(report, env, 120, 44)
    try:
        pty_.write(PROMPT)
        time.sleep(0.2)
        pty_.key("\r")

        starts = wait_events(home, "background_task_started", 180)
        report.check(
            "scenario-a",
            "the model really launched a background check",
            bool(starts),
            repr(starts)[:700],
        )
        # Arm now: the next agent request is the one after the launch, and the
        # handler will hold it until the check's terminal is durable.
        proxy.fault_armed = bool(starts)
        start = starts[0] if starts else {}
        launch_call = start.get("call_id")
        report.check(
            "scenario-a",
            "the launch names the check and its starting call",
            bool(launch_call)
            and "verify.sh" in (start.get("program", "") + " " + " ".join(start.get("args", []))),
            repr(start)[:700],
        )

        terminals = wait_events(home, "background_task_finished", 180)
        report.check(
            "scenario-a",
            "the check reached a real terminal before the recovery window",
            bool(terminals)
            and terminals[0].get("exit_code") == 0
            and terminals[0].get("status") == "Exited",
            repr(terminals)[:700],
        )
        # The check's terminal is durable; the held request now gets the 504.
        proxy.fault_gate.set()

        # The transient recovery status is presentation; the durable turn and
        # goal records are the fact. Observe the screen briefly for the frame,
        # then assert the continuation from the log.
        recovery_screen = H.wait_until(
            pty_, lambda text: "正在恢复任务" in text, timeout=30
        )
        report.dump("1-recovery", pty_)
        recovery_turns = BG.query(
            home, "select id, status, payload from turns order by ordinal"
        )
        recovery_goals = []
        for _, _, payload in recovery_turns:
            try:
                recovery_goals.append(json.loads(payload or "{}").get("goal_id"))
            except Exception:
                recovery_goals.append(None)
        recovered = (
            proxy.goal_faults == 1
            and len(recovery_turns) >= 2
            and bool(recovery_goals[0])
            and all(goal == recovery_goals[0] for goal in recovery_goals)
            and [status for _, status, _ in recovery_turns] == ["failed", "completed"]
        )
        report.check(
            "scenario-a",
            "the 504 opened a Goal continuation after the check finished",
            recovered,
            repr((recovery_screen, proxy.goal_faults, recovery_turns))[:1200],
        )

        done = H.wait_until(
            pty_,
            lambda text: "任务已完成" in text or "目标已完成" in text,
            timeout=900,
        )
        pty_.wait_stable(2.0, 90)
        report.dump("2-final", pty_)
        report.check(
            "scenario-a",
            "the goal completed and the completion reached the user",
            done,
            pty_.screen.text()[-900:],
        )

        # The check must have run once. A rerun would mean the rebuilt result
        # was not usable; the durable counts are the authority, not the prose.
        starts = event_payloads(home, "background_task_started")
        terminals = event_payloads(home, "background_task_finished")
        report.check(
            "scenario-a",
            "the background check ran exactly once and was not rerun",
            len(starts) == 1 and len(terminals) == 1,
            repr((starts, terminals))[:900],
        )
        launches = [
            call
            for call in BG.tool_calls(home)
            if call.get("name") in ("run_command", "shell_command")
            and "verify.sh" in (call.get("arguments") or "")
        ]
        report.check(
            "scenario-a",
            "the model invoked the check only once",
            len(launches) == 1,
            repr(launches)[:900],
        )

        goals = BG.settled_goal_states(home, timeout=120)
        sessions = BG.session_rows(home)
        report.check(
            "scenario-a",
            "the goal settled exactly once and the session completed",
            [state for _, state in goals] == ["settled"]
            and [row[1] for row in sessions] == ["completed"],
            repr((goals, sessions)),
        )

        terminals = BG.query(
            home,
            "select payload from events where type='task_finished' order by sequence desc limit 1",
        )
        terminal = {}
        if terminals:
            value = json.loads(terminals[0][0])
            terminal = value.get("payload", value)
        verification = terminal.get("verification") or {}
        report.check(
            "scenario-a",
            "the accepted completion is mechanically verified",
            verification.get("status") == "verified",
            repr(terminal)[:1200],
        )

        goal_calls = [
            call
            for call in BG.tool_calls(home)
            if call.get("name") == "update_goal"
        ]
        # The model may cite the command text or `call_id:<id>`; what must hold
        # is that the accepted record resolves to the call that started this
        # background execution.
        recorded_calls = [
            record.get("call_id")
            for record in (verification.get("records") or [])
        ]
        cited_call = bool(launch_call) and (
            launch_call in recorded_calls
            or any(
                ("call_id:" + launch_call) in (call.get("arguments") or "")
                for call in goal_calls
            )
        )
        report.check(
            "scenario-a",
            "the accepted completion resolves the launch call id",
            cited_call,
            repr((launch_call, recorded_calls, goal_calls[-2:]))[:1200],
        )

        turns = BG.query(
            home,
            "select id, payload from turns order by ordinal",
        )
        goal_ids = []
        for _, payload in turns:
            try:
                goal_ids.append(json.loads(payload or "{}").get("goal_id"))
            except Exception:
                goal_ids.append(None)
        report.check(
            "scenario-a",
            "the recovery is a continuation of the same goal",
            len(turns) >= 2 and bool(goal_ids[0]) and all(g == goal_ids[0] for g in goal_ids),
            repr([(row[0], goal) for row, goal in zip(turns, goal_ids)])[:900],
        )

        user_inputs = BG.query(
            home,
            "select count(*) from session_messages where payload like '%user_input%'",
        )
        report.check(
            "scenario-a",
            "the user sent one message only (no manual continue)",
            bool(user_inputs) and user_inputs[0][0] == 1,
            repr(user_inputs),
        )

        return 1 if report.failures else 0
    finally:
        pty_.close()
        report.save()
        print(
            "\n%d check(s) failed; evidence: %s"
            % (report.failures, os.path.join(H.EVIDENCE))
        )


if __name__ == "__main__":
    sys.exit(main())
