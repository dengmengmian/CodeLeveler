#!/usr/bin/env python3
"""R4: one real session with Goal/Plan, Credential Broker, steer, Prioritize and a
deterministic external blocker.

The model is real; the external service is a local HTTP server that validates a
per-run synthetic key and then deterministically refuses the second check with
503. The value reaches the service only through the authorized `.env` write; a
provider request carrying it would be a failure.

Evidence: target/r4-combined/.
Exit 0 = every check held. 2 = BLOCKED (no provider config/binary). 1 = fail.
"""

from __future__ import annotations

import http.server
import json
import os
import re
import shutil
import sqlite3
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import pty_dogfood as base  # noqa: E402
import secret_broker_real_dogfood as broker  # noqa: E402
from synthetic_canary import SyntheticCanaryRun  # noqa: E402

EVIDENCE = os.path.join(base.ROOT, "target", "r4-combined")
os.makedirs(EVIDENCE, exist_ok=True)
base.EVIDENCE = EVIDENCE
broker.EVIDENCE = EVIDENCE

RUN = SyntheticCanaryRun("r4-combined")
SEED = RUN.require(RUN.issue("seed", shape="sk"))
STEER = RUN.require(RUN.issue("steer", shape="sk"))
STEER_MARKER = "R4-STEER-7Q2"


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


def session_db(env) -> str | None:
    import glob
    paths = glob.glob(os.path.join(env.home, "state", "projects", "*", "sessions.db"))
    return paths[0] if paths else None


def query(env, sql, args=()):
    path = session_db(env)
    if not path:
        return []
    connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        return connection.execute(sql, args).fetchall()
    finally:
        connection.close()


def latest_session(env) -> str | None:
    rows = query(env, "select id from sessions order by created_at desc limit 1")
    return rows[0][0] if rows else None


def session_state(env, sid):
    rows = query(env, "select status,outcome from sessions where id=?", (sid,))
    turns = query(env, "select ordinal,status,kind from turns where session_id=?", (sid,))
    goals = query(
        env,
        "select id,state from goals where task_id in (select id from tasks where session_id=?)",
        (sid,),
    )
    return {"session": rows[0] if rows else None, "turns": turns, "goals": goals}


class BlockerService:
    """First valid check succeeds; every later valid check is deterministic 503."""

    def __init__(self):
        self.requests = []
        self.ok = 0
        self.lock = threading.Lock()
        service = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_GET(self):
                if self.path.split("?", 1)[0] != "/check":
                    self._send(404, {"status": "not_found"})
                    return
                key = self.headers.get("X-Api-Key") or ""
                with service.lock:
                    valid = key == SEED
                    if not valid:
                        status = 401
                        body = {"status": "unauthorized"}
                    elif service.ok == 0:
                        service.ok = 1
                        status = 200
                        body = {"status": "validated", "count": service.ok}
                    else:
                        status = 503
                        body = {"status": "maintenance", "count": service.ok}
                    service.requests.append(
                        {"status": status, "valid": valid, "n": len(service.requests) + 1}
                    )
                self._send(status, body)

            def _send(self, status, body):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(data)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}/check"

    def log(self) -> list[dict]:
        with self.lock:
            return list(self.requests)

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def write_fixture(repo: str, service_url: str):
    os.makedirs(os.path.join(repo, "docs"), exist_ok=True)
    script = os.path.join(repo, "check.sh")
    with open(script, "w") as handle:
        handle.write(
            "#!/bin/sh\n"
            "set -u\n"
            "KEY=$(sed -n 's/^DEMO_API_KEY=//p' .env | tail -1)\n"
            "if [ -z \"$KEY\" ]; then echo 'HTTP 0'; echo '{\"status\":\"missing_key\"}'; exit 0; fi\n"
            "BODY=$(mktemp)\n"
            f"CODE=$(curl -sS -o \"$BODY\" -w '%{{http_code}}' -H \"X-Api-Key: $KEY\" '{service_url}')\n"
            "echo \"HTTP $CODE\"\n"
            "cat \"$BODY\"\n"
            "echo\n"
            "rm -f \"$BODY\"\n"
        )
    os.chmod(script, 0o755)
    with open(os.path.join(repo, "README.md"), "w") as handle:
        handle.write("# R4 credential check fixture\n\nRun `./check.sh`; it reads DEMO_API_KEY from .env.\n")


def goal_text(repo: str) -> str:
    return (
        "在仓库中完成一次带凭据的外部服务校验，并按事实收尾。\n"
        "1. 确认 check.sh 与 docs/ 存在。\n"
        "2. 把下面的测试凭据写入 .env 的 DEMO_API_KEY；必须使用 secret_apply，"
        "不能把明文写进任何命令、日志或其他文件。\n"
        "3. 运行 ./check.sh，逐字记录第一次的 HTTP 状态与响应。\n"
        "4. 再运行一次 ./check.sh，逐字记录第二次的 HTTP 状态与响应。\n"
        "5. 如果第二次是 HTTP 503 maintenance，就把目标标记为 blocked；summary 必须写明"
        "第一次校验成功、第二次服务维护中、没有第三次重试、没有修改服务端；"
        "next_step 必须写：等待服务维护结束后重新运行 ./check.sh 并核对两次结果。\n"
        "6. 在 docs/r4-report.md 写下两次输出与结论，然后在回答中给出完整交付说明。\n"
        "7. 不要修改 check.sh 或服务端；不要用 curl 绕过 check.sh；不要伪造成功。\n"
        f"测试凭据：DEMO_API_KEY={SEED}"
    )


def steer_text() -> str:
    return (
        f"补充（运行中指令，标记 {STEER_MARKER}）：第二次出现 503 maintenance 后不要重试，"
        "不要修改服务端，也不要把失败说成完成。必须保留第一次成功的证据，"
        "并在最终回答中逐字写出两次 HTTP 状态；blocked 的 next_step 同样保留："
        "等待服务维护结束后重新运行 ./check.sh 并核对两次结果。\n"
        "凭据仍通过 secret_apply 写入 .env，不得把明文放进命令或其他文件。\n"
        f"另一个测试标记 DEMO_STEER_KEY={STEER} 仅用于确认收到这次追加：不要写入任何文件，"
        "也不要在回答里复述明文。\n"
        "请先把这条追加纳入当前目标，再继续执行；完成后仍按原要求写 docs/r4-report.md。\n"
        "这次追加较长，是为了验证运行中长文本不会丢失、不会被拆成重复指令，也不会覆盖原目标。"
    )


def provider_bodies(proxy) -> str:
    return proxy.body_text()


def sanitize_evidence():
    """Remove any synthetic credential a terminal dump may have wrapped."""
    import glob

    patterns = [
        re.compile(rb"sk-[A-Za-z0-9]{40}"),
        re.compile(rb"CLTEST-[0-9a-f]{8}-[a-z0-9-]+-(?:\s*[A-Za-z0-9]){40}"),
    ]
    for path in glob.glob(os.path.join(EVIDENCE, "*")):
        if not os.path.isfile(path) or not path.endswith((".json", ".txt", ".raw")):
            continue
        try:
            data = open(path, "rb").read()
        except OSError:
            continue
        cleaned = data
        for pattern in patterns:
            cleaned = pattern.sub(b"<SYNTHETIC-CANARY-REMOVED>", cleaned)
        if cleaned != data:
            open(path, "wb").write(cleaned)


def main() -> int:
    if not os.path.exists(base.BIN):
        print(f"missing {base.BIN} — build it first")
        return 2
    source_config = os.path.expanduser("~/.leveler/config.toml")
    if not os.path.exists(source_config):
        print("BLOCKED: no ~/.leveler/config.toml for a real provider run")
        return 2
    provider, upstream = broker.config_provider(source_config)
    proxy = broker.RecordingProxy(upstream)
    service = BlockerService()
    env = base.Env("real", None)
    env.repo = os.path.realpath(env.repo)
    broker.rewrite_base_url(os.path.join(env.home, "config.toml"), provider, proxy.base_url())
    write_fixture(env.repo, service.url())
    report = base.Report("r4-combined")
    report.check("env", f"a real provider is configured ({provider})", True)
    pty = None
    try:
        pty = base.boot(report, env, 132, 46, extra=["--model", "deepseek/deepseek-v4.1-flash-1017"])
        pty.write("/goal " + goal_text(env.repo))
        time.sleep(0.5)
        pty.key("\r")
        review = wait(pty, lambda: "敏感凭据" in pty.screen.text(), 40)
        report.check(
            "credential", "the goal opens the credential review", review, pty.screen.text()[-600:]
        )
        if not review:
            report.save()
            return 1
        report.check(
            "credential", "the review never paints the value", SEED not in pty.screen.text()
        )
        pty.key("\r")  # secure capture

        # Wait until the goal is actually running before the steer.
        busy = wait(
            pty,
            lambda: any(
                word in flat(pty)
                for word in ("运行中", "处理中", "思考中", "生成中", "等待模型", "Esc 打断", "执行了")
            ),
            120,
        )
        report.check("steer", "the Goal turn is running before the steer", busy, pty.screen.text()[-500:])
        pty.write(steer_text())
        time.sleep(0.4)
        pty.key("\r")
        steer_review = wait(pty, lambda: "敏感凭据" in pty.screen.text(), 40)
        report.check(
            "steer", "the long steer opens its own credential review", steer_review,
            pty.screen.text()[-600:],
        )
        if steer_review:
            report.check(
                "steer", "the steer review never paints the value", STEER not in pty.screen.text()
            )
            pty.key("\r")
        # Prioritize: open the pending-input screen and send everything now.
        time.sleep(1.0)
        pty.key("\x1bs")  # Alt+S opens pending steer
        time.sleep(0.4)
        opened = "待发送" in flat(pty) or "立即发送" in flat(pty)
        report.check("prioritize", "the pending-input screen is reachable", opened, pty.screen.text()[-500:])
        if opened:
            pty.write("a")
            time.sleep(0.5)
        # Leave the pending-input screen so the conversation (and the final
        # answer) is what the terminal shows.
        pty.key("\x1b")
        time.sleep(0.5)
        report.dump("10-live", pty)

        # Durable terminal: the goal must settle blocked, and the service must
        # have observed exactly the two checks the task asked for.
        sid = latest_session(env)
        terminal = None
        deadline = time.time() + 1200
        while time.time() < deadline:
            state = session_state(env, sid)
            if (
                state["session"]
                and state["session"][0] in ("blocked", "completed", "failed", "interrupted")
                and all(turn[1] != "running" for turn in state["turns"])
            ):
                terminal = state
                break
            if not pty.alive():
                break
            time.sleep(2)
        if "待发送" in flat(pty) or "立即发送" in flat(pty):
            pty.key("\x1b")
            time.sleep(0.5)
        wait(pty, lambda: "※ 回顾" in pty.screen.text(), 30)
        time.sleep(2)
        report.dump("20-final", pty)
        state = session_state(env, sid)
        report.check("goal", "the session reached a terminal state", terminal is not None, json.dumps(state))
        report.check(
            "goal",
            "the goal settled blocked (a maintenance dependency, not success)",
            state["session"] is not None and state["session"][0] == "blocked",
            json.dumps(state),
        )
        service_log = service.log()
        report.check(
            "service",
            "the authorized credential validated once and the second check was 503",
            [entry["status"] for entry in service_log] == [200, 503],
            json.dumps(service_log),
        )
        envfile = os.path.join(env.repo, ".env")
        dotenv = open(envfile).read() if os.path.exists(envfile) else ""
        report.check(
            "credential",
            "the authorized secret_apply wrote the value to .env",
            f"DEMO_API_KEY={SEED}" in dotenv,
            "the .env write is missing",
        )
        bodies = provider_bodies(proxy)
        report.check(
            "wire",
            "no provider request carried either plaintext value",
            SEED not in bodies and STEER not in bodies,
            "a provider request carried a plaintext credential",
        )
        report.check(
            "wire",
            "provider requests carry an opaque secret reference",
            re.search(r"sec_[0-9a-f]{32}", bodies) is not None,
            "no reference reached the model",
        )
        report.check(
            "steer",
            "the long steer reached the running turn exactly once",
            bodies.count(STEER_MARKER) >= 1,
            f"marker occurrences: {bodies.count(STEER_MARKER)}",
        )
        final_screen = pty.screen.text()
        delivery_rows = query(
            env,
            "select payload from events where session_id=? and type='delivery_committed'",
            (sid,),
        )
        delivery_text = ""
        if delivery_rows:
            try:
                delivery_text = json.loads(delivery_rows[-1][0]).get("payload", {}).get("text", "")
            except (TypeError, ValueError):
                delivery_text = ""
        report.check(
            "delivery",
            "the committed delivery preserves both HTTP outcomes",
            "200" in delivery_text and "503" in delivery_text,
            delivery_text[-900:],
        )
        report.check(
            "recap", "the recap row is still present", "※ 回顾" in final_screen,
            final_screen[-700:],
        )
        report.check(
            "blocked",
            "the answer preserves the first success and names the second failure",
            "200" in delivery_text and "503" in delivery_text and "blocked" in delivery_text,
            delivery_text[-900:],
        )

        # Resume the same blocked session through the product's own path.
        pty.close()
        pty = None
        time.sleep(1.0)
        pty = base.boot(
            report,
            env,
            132,
            46,
            extra=["--model", "deepseek/deepseek-v4.1-flash-1017", "--session", sid],
        )
        resumed = wait(pty, lambda: "blocked" in flat(pty) or "目标受阻" in flat(pty), 30)
        after_resume = session_state(env, sid)
        report.check(
            "resume",
            "the blocked goal reopens with its history and state",
            resumed and after_resume["session"][0] == "blocked",
            json.dumps(after_resume) + "\n" + pty.screen.text()[-600:],
        )
        report.check(
            "resume",
            "resuming did not execute the checks again",
            [entry["status"] for entry in service.log()] == [200, 503],
            json.dumps(service.log()),
        )
        report.dump("30-resumed", pty)

        summary = {
            "provider": provider,
            "model": "deepseek/deepseek-v4.1-flash-1017",
            "session": sid,
            "state": state,
            "service": service.log(),
            "canary": RUN.manifest(),
            "checks": report.results,
        }
        with open(os.path.join(EVIDENCE, "r4-combined.json"), "w") as handle:
            json.dump(summary, handle, ensure_ascii=False, indent=2)
        report.save()
        sanitize_evidence()
        print(json.dumps({"failures": report.failures, "session": sid, "service": service.log()}, ensure_ascii=False))
        return 1 if report.failures else 0
    finally:
        if pty is not None:
            pty.close()
        service.close()
        proxy.close()
        sanitize_evidence()


if __name__ == "__main__":
    sys.exit(main())
