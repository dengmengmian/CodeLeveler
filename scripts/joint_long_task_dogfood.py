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
    "/goal 在仓库根目录创建 joint-final.txt，内容正好一行 ok。用工具完成；"
    "最终回答必须包含标记 JOINT-FINAL-MARKER，并说明你收到了编辑后的补充要求。"
)
ALPHA = "STEER_ALPHA_ORIGINAL"
BETA = "STEER_BETA_EDITED"
WITHDRAW = "STEER_WITHDRAW_ME"


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

        # Send now: the priority interrupts the held attempt, and the next
        # round carries the edited text.
        pty_.key("\x1bs")
        wait("待投递追加指令", 20)
        pty_.key("a")
        wait("等待安全投递", 20)
        pty_.key("\x1b")
        proxy.hold_release.set()

        completed = H.wait_until(
            pty_,
            lambda text: "JOINT-FINAL-MARKER" in text and "已完成" in text,
            timeout=600,
        )
        pty_.wait_stable(2.0, 60)
        report.dump("2-final", pty_)
        screen = pty_.screen.text()
        report.check("joint", "the goal completed with the required marker", completed, screen[-900:])

        bodies = [body.decode("utf-8", "replace") for body in proxy.request_bodies]
        joined = "\n".join(bodies)
        report.check("joint", "the 504 really happened once", proxy.goal_faults == 1, str(proxy.goal_faults))
        report.check(
            "joint",
            "the recovery window really was held",
            proxy.held.is_set(),
            "no hold",
        )
        report.check(
            "joint",
            "the edited revision reached a provider request once",
            any(body.count(BETA) == 1 for body in bodies),
            "BETA not delivered once",
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
        rows = GF.rows(home, "SELECT payload FROM events WHERE type='task_finished'")
        report.check("joint", "the goal reached a completed terminal", bool(rows), repr(rows[:1]))
    finally:
        proxy.hold_release.set()
        pty_.close()
        proxy.shutdown()
    report.save()
    print("assertions: %d, failures: %d" % (len(report.results), report.failures))
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
