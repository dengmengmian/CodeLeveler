"""One real long task: Goal recovery × append × edit × withdraw × send-now.

The provider is the machine's real one, reached through the local forwarding
proxy in `goal_fault_dogfood.py`. The proxy answers the FIRST agent request
with a complete HTTP 504 (Goal-level recovery) and HOLDS the second (the
recovery window) until this harness has driven real TUI actions. Everything
else — the runtime, the goal lifecycle, the input journal, the TUI, the
terminal, the model — is real.

Sequence:
  /goal task -> 504 -> Goal recovery window (held)
  -> append ALPHA -> edit it to BETA -> append WITHDRAW -> withdraw it
  -> send-now (prioritize BETA; it interrupts the held attempt)
  -> the next round carries BETA -> the model completes -> final answer.

Evidence is written under test-output/unified-final/joint-long/.
Exit 0 means every assertion held.
"""

import json
import os
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import goal_fault_dogfood as GF  # noqa: E402
import pty_dogfood as H  # noqa: E402

OUT = Path(sys.argv[1] if len(sys.argv) > 1 else "test-output/unified-final/joint-long").resolve()
OUT.mkdir(parents=True, exist_ok=True)
H.EVIDENCE = str(OUT)

TASK = (
    "/goal 在仓库根目录创建 joint-final.txt，内容正好一行 ok。然后用前台工具运行下面这一条命令并等它结束："
    "sh -c 'until [ -f joint-gate-release ]; do sleep 0.1; done; echo GATE-OK'。"
    "这条命令必须用 run_command 前台运行：background 为 false，timeout_seconds 设为 120（不要用 0 或省略）。"
    "不要自己创建 joint-gate-release，也不要把它移到后台；只有我会创建这个文件。"
    "命令结束后给出最终回答；最终回答必须包含标记 JOINT-FINAL-MARKER，并说明你收到了编辑后的补充要求。"
)
ALPHA = "STEER_ALPHA_ORIGINAL"
BETA = "STEER_BETA_EDITED"
WITHDRAW = "STEER_WITHDRAW_ME"
GATE_WAIT = "STEER_GATE_WAIT"
GATE = "joint-gate-release"
GATE_COMMAND = "until [ -f joint-gate-release ]"


def wait_for_consumed(home: str, needle: str, timeout: float = 120) -> bool:
    """Wait until every admitted row carrying `needle` has been consumed."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        rows = GF.rows(
            home,
            "SELECT state FROM session_inputs WHERE content LIKE '%%%s%%'" % needle,
        )
        if rows and all(row["state"] == "consumed" for row in rows):
            return True
        time.sleep(0.05)
    return False


def _calls_gate(payload: dict) -> bool:
    """Whether one command-tool payload embeds the controlled gate command.

    Match the call's real arguments, and only command tools: a reason, a
    scope declaration, or a completion citation that merely quotes the task
    must not look like the boundary.
    """
    if payload.get("name") not in ("run_command", "shell_command"):
        return False
    arguments = payload.get("arguments")
    if isinstance(arguments, str):
        try:
            arguments = json.loads(arguments)
        except ValueError:
            return GATE_COMMAND in arguments
    return GATE_COMMAND in json.dumps(arguments, ensure_ascii=False)


def running_gate_calls(home: str) -> list[str]:
    """Call ids whose gate command started and has not finished yet.

    The screen collapses a running tool's argv, so the durable dispatch rows
    are the authority for "the tool boundary is real right now", not a guess
    from rendered text.
    """
    started: list[str] = []
    finished: set[str] = set()
    for row in GF.rows(
        home,
        "SELECT type, payload FROM events WHERE type IN "
        "('tool_call_started','tool_call_finished')",
    ):
        try:
            payload = json.loads(row["payload"]).get("payload") or {}
        except (TypeError, ValueError):
            continue
        if not _calls_gate(payload):
            continue
        call_id = payload.get("call_id")
        if not call_id:
            continue
        if row["type"] == "tool_call_started":
            started.append(call_id)
        else:
            finished.add(call_id)
    return [call_id for call_id in started if call_id not in finished]


def gate_dispatches(home: str) -> list[str]:
    """Every gate command dispatch on the durable record."""
    calls: list[str] = []
    for row in GF.rows(
        home,
        "SELECT payload FROM events WHERE type='tool_call_started'",
    ):
        try:
            payload = json.loads(row["payload"]).get("payload") or {}
        except (TypeError, ValueError):
            continue
        if _calls_gate(payload) and payload.get("call_id"):
            calls.append(payload["call_id"])
    return calls


def user_message_rows(home: str, needle: str) -> list[dict]:
    """User-role transcript rows carrying `needle` (assistant quotes excluded)."""
    found = []
    for row in GF.rows(
        home,
        "SELECT payload FROM session_messages WHERE payload LIKE '%%%s%%'" % needle,
    ):
        try:
            payload = json.loads(row["payload"])
        except (TypeError, ValueError):
            continue
        if payload.get("role") == "user":
            found.append(payload)
    return found


def wait_for_running_gate(home: str, timeout: float = 240) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if running_gate_calls(home):
            return True
        time.sleep(0.05)
    return False


def main() -> int:
    source = os.path.expanduser("~/.leveler/config.toml")
    import re

    upstream = re.search(r'base_url\s*=\s*"(https://[^"]+)"', open(source).read())
    if upstream is None:
        raise SystemExit("the machine config names no provider base_url")
    proxy = GF.Proxy(upstream.group(1))
    proxy.fault_mode = "goal"
    proxy.hold_release = threading.Event()
    threading.Thread(target=proxy.serve_forever, daemon=True).start()

    env = H.Env("real", None)
    home, repo = env.home, env.repo
    GF.patch_config(home, proxy.port)
    report = H.Report("joint-long-real")
    pty_ = H.boot(report, env, 120, 44)

    def wait(needle, timeout=180):
        ok = H.wait_until(pty_, lambda text: needle in text, timeout=timeout)
        report.check("joint", f"reaches {needle!r}", ok, pty_.screen.text()[-700:])
        return ok

    try:
        pty_.write(TASK)
        time.sleep(0.2)
        pty_.key("\r")
        # The first request is faulted; the recovery window is held open.
        held = proxy.held.wait(180)
        report.check("joint", "the recovery window is held open for real UI actions", held)
        report.dump("1-recovery-held", pty_)
        wait("正在恢复任务", 30)

        pty_.write(ALPHA)
        time.sleep(0.15)
        pty_.key("\r")
        wait("等待投递", 20)

        # Edit ALPHA -> BETA through the real detail view and composer.
        pty_.key("\x1bs")
        wait("待投递追加指令", 20)
        pty_.key("e")
        wait("正在编辑追加指令", 20)
        pty_.key("\x15")
        pty_.write(BETA)
        time.sleep(0.2)
        pty_.key("\r")
        wait(BETA, 20)

        # A second append, then withdraw it.
        pty_.write(WITHDRAW)
        time.sleep(0.15)
        pty_.key("\r")
        wait("2 条等待投递", 20)
        pty_.key("\x1bs")
        wait("待投递追加指令", 20)
        pty_.key("\x1b[B", "\x1b[3~")
        gone = H.wait_until(pty_, lambda text: WITHDRAW not in text, timeout=20)
        report.check("joint", "the withdrawn append left the screen", gone)
        pty_.key("\x1b")
        time.sleep(0.3)

        # Release the recovery window so the model reaches a REAL foreground
        # tool execution. A held model call is not a safe-delivery boundary:
        # a priority steer interrupts that call and can be delivered before
        # the next frame is painted, so asserting the waiting label there is
        # asserting a race, not a state. The gate command below is the
        # controlled boundary where "waiting for safe delivery" is true.
        proxy.hold_release.set()
        running = wait_for_running_gate(home)
        report.check(
            "joint",
            "the controlled gate tool is really running",
            running,
            pty_.screen.text()[-700:],
        )
        if not running:
            raise RuntimeError("the model never started the controlled gate command")

        # The edited steer admitted during the held recovery request was
        # already delivered by the model's own next round. That is the
        # "delivered" state, not a waiting one; assert it is gone before the
        # boundary test below creates the only pending row.
        consumed = wait_for_consumed(home, BETA)
        report.check(
            "joint",
            "the edited steer was delivered on the model's next round",
            consumed,
            pty_.screen.text()[-700:],
        )

        # A NEW steer while the tool genuinely executes. Only now does
        # "send now" have a safe delivery to wait for: the running tool keeps
        # the current round open, so the row cannot be consumed until it ends.
        pty_.write(GATE_WAIT)
        time.sleep(0.15)
        pty_.key("\r")
        wait("等待投递", 20)
        pty_.key("\x1bs")
        wait("待投递追加指令", 20)
        pty_.key("a")
        wait("等待安全投递", 20)
        report.dump("2-safe-wait", pty_)
        boundary_samples = []
        for _ in range(10):
            boundary_samples.append(pty_.screen.text())
            time.sleep(0.15)
        boundary_running = bool(running_gate_calls(home))
        report.check(
            "joint",
            "the gate tool is still executing at the sampled boundary",
            boundary_running,
            pty_.screen.text()[-900:],
        )
        report.check(
            "joint",
            "the priority row waits while the tool is still executing",
            boundary_running
            and all("等待安全投递" in frame for frame in boundary_samples),
            repr(
                [
                    ("等待安全投递" in frame, frame[-300:])
                    for frame in boundary_samples
                ]
            ),
        )
        # Releasing the tool is the safe boundary; the next round carries the
        # edited text. Stay on the detail screen: it is the surface whose
        # post-delivery state this scenario asserts.
        Path(repo, GATE).write_text("release\n")
        delivered = H.wait_until(
            pty_,
            lambda text: "暂无待投递的追加指令" in text,
            timeout=600,
        )
        report.check(
            "joint",
            "the pending area is empty after safe delivery",
            delivered,
            pty_.screen.text()[-900:],
        )
        report.dump("2-delivered", pty_)
        stale = H.wait_until(
            pty_,
            lambda text: "等待安全投递" in text or "等待投递" in text,
            timeout=1,
        )
        report.check(
            "joint",
            "no stale waiting hint survives delivery",
            not stale,
            pty_.screen.text()[-900:],
        )
        pty_.key("\x1b")

        completed = H.wait_until(
            pty_,
            lambda text: "JOINT-FINAL-MARKER" in text and "已完成" in text,
            timeout=600,
        )
        pty_.wait_stable(2.0, 60)
        report.dump("2-final", pty_)
        screen = pty_.screen.text()
        report.check("joint", "the goal completed with the required marker", completed, screen[-900:])
        report.check(
            "joint",
            "the conversation carries no stale pending-area copy",
            "等待安全投递" not in screen and "等待投递" not in screen,
            screen[-900:],
        )

        bodies = [body.decode("utf-8", "replace") for body in proxy.request_bodies]
        joined = "\n".join(bodies)
        report.check("joint", "the 504 really happened once", proxy.goal_faults == 1, str(proxy.goal_faults))
        report.check(
            "joint",
            "the recovery window really was held",
            proxy.held.is_set(),
            "no hold",
        )
        first = next((index for index, body in enumerate(bodies) if BETA in body), None)
        report.check(
            "joint",
            "the edited revision is introduced into exactly one round",
            first is not None
            and bodies[first].count(BETA) == 1
            and all(BETA not in body for body in bodies[:first]),
            repr([body.count(BETA) for body in bodies]),
        )
        gate_first = next((index for index, body in enumerate(bodies) if GATE_WAIT in body), None)
        report.check(
            "joint",
            "the boundary steer is introduced into exactly one round",
            gate_first is not None
            and bodies[gate_first].count(GATE_WAIT) == 1
            and all(GATE_WAIT not in body for body in bodies[:gate_first]),
            repr([body.count(GATE_WAIT) for body in bodies]),
        )
        report.check(
            "joint",
            "the original and withdrawn texts never reached a request",
            ALPHA not in joined and WITHDRAW not in joined,
            "a replaced/withdrawn text leaked",
        )
        report.check(
            "joint",
            "the file exists with exactly the requested line",
            Path(repo, "joint-final.txt").exists()
            and Path(repo, "joint-final.txt").read_text() == "ok\n",
            repr(Path(repo, "joint-final.txt").read_text() if Path(repo, "joint-final.txt").exists() else None),
        )
        user_inputs = GF.rows(
            home,
            "SELECT payload FROM session_messages WHERE payload LIKE '%user_input%'",
        )
        report.check(
            "joint",
            "no manual continue was ever typed",
            all("继续" not in (row["payload"] or "") for row in user_inputs),
            repr([row["payload"][:80] for row in user_inputs]),
        )
        report.check("joint", "no failure card survived", "请求失败" not in screen, screen[-700:])
        report.check(
            "joint",
            "no goal-control row reached the conversation",
            "目标收尾" not in screen and "目标未更新" not in screen,
            screen[-700:],
        )
        terminals = []
        for row in GF.rows(home, "SELECT payload FROM events WHERE type='task_finished'"):
            try:
                payload = json.loads(row["payload"]).get("payload") or {}
            except (TypeError, ValueError):
                continue
            terminals.append(payload.get("outcome"))
        report.check(
            "joint",
            "one failed old-window terminal and one completed final terminal",
            terminals.count("failed") == 1 and terminals.count("completed") == 1,
            repr(terminals),
        )
        report.check(
            "joint",
            "the controlled gate command dispatched exactly once",
            len(gate_dispatches(home)) == 1,
            repr(gate_dispatches(home)),
        )
        journal = GF.rows(
            home,
            "SELECT state, revision, content FROM session_inputs "
            "WHERE content LIKE '%STEER_BETA_EDITED%'",
        )
        report.check(
            "joint",
            "the edited steer has exactly one consumed journal row",
            len(journal) == 1 and journal[0]["state"] == "consumed",
            repr(journal),
        )
        history = user_message_rows(home, BETA)
        report.check(
            "joint",
            "the edited steer appears exactly once in user history",
            len(history) == 1,
            repr([json.dumps(row, ensure_ascii=False)[:120] for row in history]),
        )
        gate_journal = GF.rows(
            home,
            "SELECT state, revision, content FROM session_inputs "
            "WHERE content LIKE '%STEER_GATE_WAIT%'",
        )
        report.check(
            "joint",
            "the boundary steer has exactly one consumed journal row",
            len(gate_journal) == 1 and gate_journal[0]["state"] == "consumed",
            repr(gate_journal),
        )
        gate_history = user_message_rows(home, GATE_WAIT)
        report.check(
            "joint",
            "the boundary steer appears exactly once in user history",
            len(gate_history) == 1,
            repr([json.dumps(row, ensure_ascii=False)[:120] for row in gate_history]),
        )
    finally:
        proxy.hold_release.set()
        pty_.close()
        proxy.shutdown()
        report.save()
    print("assertions: %d, failures: %d" % (len(report.results), report.failures))
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
