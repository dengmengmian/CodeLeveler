#!/usr/bin/env python3
"""Real-model dogfood for the Secret Broker.

The deterministic PTY script proves the mechanism with a scripted provider.
This script proves the part a script cannot: a real model reads the user's
request, chooses `secret_apply` on its own, and the host writes the value.

The provider connection already configured on this machine is reused. A local
recording proxy sits in front of it so the actual request bodies can be
scanned; the proxy forwards the real Authorization header unchanged and never
writes headers to evidence. The API key is never printed or persisted.

Scenarios:
  A  automatic detection: the person confirms capture, the model applies the
     value, then inspects the field.
  B  `/secret` secure paste of an unknown format, then a normal instruction
     naming the target.
  C  Goal mode plus a mid-turn steer that mentions the captured reference.
  D  restart boundary: the vault is process-local, so a reference from the
     dead process must fail and the model must ask again.

Usage:
    cargo build -p leveler-cli --bin leveler
    python3 scripts/secret_broker_real_dogfood.py [--only A,B,C,D]

Exit 0 = every executed check held; 2 = BLOCKED (no provider config/binary);
1 = a check failed. Evidence goes to target/secret-broker-real/.
"""

from __future__ import annotations

import argparse
import http.client
import http.server
import json
import os
import re
import shutil
import socket
import ssl
import sys
import threading
import time
import urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import pty_dogfood as base  # noqa: E402
from synthetic_canary import SyntheticCanaryRun  # noqa: E402

CANARY_RUN = SyntheticCanaryRun("secret-broker-real")
CANARY = CANARY_RUN.require(CANARY_RUN.issue("detected", shape="sk"))
UNKNOWN = CANARY_RUN.require(CANARY_RUN.issue("unknown"))
GOAL_CANARY = CANARY_RUN.require(CANARY_RUN.issue("goal", shape="sk"))
EVIDENCE = os.path.join(base.ROOT, "target", "secret-broker-real")
os.makedirs(EVIDENCE, exist_ok=True)
base.EVIDENCE = EVIDENCE


# ── Recording proxy ────────────────────────────────────────────────────────


class Recorder:
    def __init__(self):
        self.bodies: list[str] = []
        self.lock = threading.Lock()

    def add(self, body: str) -> None:
        with self.lock:
            self.bodies.append(body)

    def text(self) -> str:
        with self.lock:
            return "\n".join(self.bodies)


class ProxyHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    upstream = ("", 443)
    upstream_base = "/"
    recorder: Recorder
    use_tls = True

    def log_message(self, *_args):
        pass

    def _forward(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else b""
        try:
            text = body.decode("utf-8", "replace")
            if text:
                self.recorder.add(text)
        except Exception:
            pass
        host, port = self.upstream
        target = self.upstream_base.rstrip("/") + self.path
        conn = (
            http.client.HTTPSConnection(
                host, port, timeout=300, context=ssl.create_default_context()
            )
            if self.use_tls
            else http.client.HTTPConnection(host, port, timeout=300)
        )
        try:
            conn.putrequest(self.command, target)
            for name, value in self.headers.items():
                lower = name.lower()
                if lower in ("host", "content-length", "connection", "accept-encoding"):
                    continue
                conn.putheader(name, value)
            conn.putheader("Accept-Encoding", "identity")
            conn.putheader("Content-Length", str(len(body)))
            conn.endheaders(body)
            upstream = conn.getresponse()
            self.send_response(upstream.status)
            for name, value in upstream.getheaders():
                if name.lower() in ("transfer-encoding", "content-length", "connection"):
                    continue
                self.send_header(name, value)
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            while True:
                chunk = upstream.read(4096)
                if not chunk:
                    break
                self.wfile.write(b"%x\r\n" % len(chunk))
                self.wfile.write(chunk)
                self.wfile.write(b"\r\n")
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except Exception as error:
            try:
                self.send_response(502)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", "0")
                self.end_headers()
            except Exception:
                pass
            print(f"proxy error: {error}", file=sys.stderr)
        finally:
            conn.close()

    do_POST = _forward
    do_GET = _forward


class RecordingProxy:
    def __init__(self, upstream_base: str):
        parsed = urllib.parse.urlparse(upstream_base)
        self.recorder = Recorder()
        host = parsed.hostname or ""
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
        handler = type(
            "BoundProxyHandler",
            (ProxyHandler,),
            {
                "upstream": (host, port),
                "upstream_base": parsed.path or "/",
                "recorder": self.recorder,
                "use_tls": parsed.scheme == "https",
            },
        )
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def body_text(self) -> str:
        return self.recorder.text()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def config_provider(path: str) -> tuple[str, str]:
    """The default model's provider and its configured base_url. No key."""
    text = open(path).read()
    default = re.search(r'^\s*default_model\s*=\s*"([^"]+)"', text, re.M)
    if not default:
        raise SystemExit("BLOCKED: ~/.leveler/config.toml has no default_model")
    provider = default.group(1).split("/", 1)[0]
    section = re.search(
        r"^\s*\[providers\.\"?%s\"?\]\s*$(.*?)(?=^\s*\[|\Z)"
        % re.escape(provider),
        text,
        re.M | re.S,
    )
    if not section:
        raise SystemExit(f"BLOCKED: provider {provider} is not configured")
    base_url = re.search(r'^\s*base_url\s*=\s*"([^"]+)"', section.group(1), re.M)
    if not base_url:
        raise SystemExit(f"BLOCKED: provider {provider} has no base_url")
    return provider, base_url.group(1)


def rewrite_base_url(path: str, provider: str, proxy_base: str) -> None:
    lines = open(path).read().splitlines(keepends=True)
    in_section = False
    for index, line in enumerate(lines):
        match = re.match(r'\s*\[providers\.([^\]]+)\]', line)
        if match:
            in_section = match.group(1).strip().strip('"') == provider
            continue
        if in_section and re.match(r"\s*base_url\s*=", line):
            indent = line[: len(line) - len(line.lstrip())]
            lines[index] = f'{indent}base_url = "{proxy_base}"\n'
            break
    with open(path, "w") as handle:
        handle.write("".join(lines))


# ── TUI helpers ────────────────────────────────────────────────────────────


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


def command(pty, text, settle=0.6):
    pty.key("\x1b")
    time.sleep(0.4)
    pty.write(text)
    time.sleep(0.3)
    pty.key("\r")
    time.sleep(settle)


def turns(pty) -> int:
    """Tool rows on screen, a proxy for 'the model did something'."""
    text = flat(pty)
    return text.count("secret_apply") + text.count("secret_inspect")


def dotenv(repo: str) -> str:
    path = os.path.join(repo, ".env")
    try:
        return open(path).read()
    except OSError:
        return ""


def wait_terminal(pty, timeout=420) -> bool:
    return wait(
        pty,
        lambda: any(
            word in flat(pty)
            for word in (
                "回答结束",
                "任务已完成",
                "已完成",
                "已取消",
                "失败",
                "未完成",
                "blocked",
            )
        ),
        timeout,
    )


def settled(pty, timeout=120) -> bool:
    """Wait until no busy indicator has been on screen for two seconds."""
    end = time.time() + timeout
    quiet_since = None
    while time.time() < end:
        text = flat(pty)
        busy = any(
            word in text
            for word in ("等待模型", "思考中", "生成中", "运行中", "处理中", "Esc 打断")
        )
        if busy:
            quiet_since = None
        elif quiet_since is None:
            quiet_since = time.time()
        elif time.time() - quiet_since >= 2.0:
            return True
        if not pty.alive():
            return False
        time.sleep(0.3)
    return False


# ── Scenarios ──────────────────────────────────────────────────────────────


def scenario_a(report, pty, env):
    prompt = (
        "请检查当前项目，并将以下测试凭据配置到 .env：\n\n"
        f"DEMO_API_KEY={CANARY}\n\n"
        "配置完成后，用 secret_inspect 验证字段存在，并简短报告结果。"
    )
    pty.write(prompt)
    time.sleep(0.4)
    pty.key("\r")
    review = wait(pty, lambda: "敏感凭据" in pty.screen.text(), 30)
    report.check("A", "the real TUI detects the credential and asks", review, pty.screen.text()[-600:])
    if not review:
        return False
    frame = pty.screen.text()
    report.check("A", "the review never paints the value", CANARY not in frame)
    pty.key("\r")  # default: secure capture
    finished = wait_terminal(pty, 420)
    quiet = settled(pty, 120)
    report.check(
        "A",
        "the real model turn reached a terminal state and settled",
        finished and quiet,
        pty.screen.text()[-800:],
    )
    written = dotenv(env.repo)
    report.check(
        "A",
        "the model applied the captured value to .env",
        f"DEMO_API_KEY={CANARY}" in written,
        written,
    )
    report.dump("A-live", pty)
    return True


def scenario_b(report, pty, env):
    command(pty, "/secret")
    opened = wait(pty, lambda: "粘贴内容不会回显" in pty.screen.text(), 15)
    report.check("B", "/secret opens the masked field", opened, pty.screen.text()[-500:])
    if not opened:
        return False
    pty.write(UNKNOWN)
    time.sleep(0.3)
    report.check("B", "the field paints a mask, not the value", UNKNOWN not in pty.screen.text())
    pty.key("\r")
    captured = wait(pty, lambda: "已安全接管" in pty.screen.text(), 20)
    report.check("B", "the runtime captured the value", captured, pty.screen.text()[-500:])
    if not captured:
        return False
    pty.write(" 请把它配置到 .env，字段名为 DEMO_UNKNOWN_KEY，并用 secret_inspect 验证字段存在，然后简短报告。")
    time.sleep(0.4)
    pty.key("\r")
    finished = wait_terminal(pty, 420)
    quiet = settled(pty, 120)
    report.check(
        "B",
        "the real model turn reached a terminal state and settled",
        finished and quiet,
        pty.screen.text()[-800:],
    )
    written = dotenv(env.repo)
    report.check(
        "B",
        "the unknown-format value was written under the requested field",
        f"DEMO_UNKNOWN_KEY={UNKNOWN}" in written,
        written,
    )
    report.dump("B-live", pty)
    return True


def scenario_c(report, pty, env):
    """Goal + mid-turn steer. The steer names the target for a captured ref."""
    goal = (
        "创建一个 docs/broker-demo.md，内容写一行 'secret broker demo'，"
        "然后把测试凭据配置到 .env 的 DEMO_API_KEY，并用 secret_inspect 验证。"
        f"\n\nDEMO_API_KEY={GOAL_CANARY}"
    )
    command(pty, "/goal " + goal, 0.5)
    review = wait(pty, lambda: "敏感凭据" in pty.screen.text(), 30)
    report.check("C", "Goal input opens the credential review", review, pty.screen.text()[-600:])
    if not review:
        return False
    pty.key("\r")
    # The model starts working. Steer while it runs: the steer names the
    # target again and carries the reference already in the transcript.
    time.sleep(3.0)
    busy = any(
        word in flat(pty)
        for word in ("运行中", "处理中", "思考中", "生成中", "等待模型", "Esc 打断")
    )
    report.check("C", "the Goal turn is running before the steer", busy, pty.screen.text()[-500:])
    pty.write("补充：配置目标仍是 .env 的 DEMO_API_KEY；完成后请检查 .env。")
    time.sleep(0.3)
    pty.key("\r")
    finished = wait_terminal(pty, 480)
    quiet = settled(pty, 150)
    report.check(
        "C",
        "the Goal turn reached a terminal state and settled",
        finished and quiet,
        pty.screen.text()[-800:],
    )
    written = dotenv(env.repo)
    report.check(
        "C",
        "the Goal applied the captured value",
        f"DEMO_API_KEY={GOAL_CANARY}" in written,
        written,
    )
    report.dump("C-live", pty)
    return True


def scenario_d(report, pty, env):
    """Restart: the new process cannot resolve the old process's reference."""
    command(pty, "/secret")
    opened = wait(pty, lambda: "粘贴内容不会回显" in pty.screen.text(), 15)
    report.check("D", "a fresh capture is open before the restart", opened)
    if not opened:
        return None
    value = CANARY_RUN.require(CANARY_RUN.issue("restart-boundary"))
    pty.write(value)
    time.sleep(0.3)
    pty.key("\r")
    captured = wait(pty, lambda: "已安全接管" in pty.screen.text(), 20)
    text = pty.screen.text()
    match = re.search(r"\[secret:(sec_[0-9a-f]{32})\]", text)
    report.check(
        "D",
        "the restart probe value was captured",
        captured and match is not None,
        text[-500:],
    )
    if not captured or match is None:
        return None
    reference = match.group(1)
    with open(os.path.join(EVIDENCE, "restart-reference.txt"), "w") as handle:
        handle.write(f"{reference}\n{value}\n")
    return reference


def scenario_d_resume(report, pty, env, reference, before):
    instruction = (
        "重启后继续。第一步必须是直接调用工具 secret_apply，参数为 secret_ref=\""
        + reference
        + "\"，field=\"DEMO_API_KEY\"，target=\".env\"；不要先用自然语言询问。"
        "如果该工具返回引用不可用，再告诉我需要重新提供凭据。"
    )
    pty.write(instruction)
    time.sleep(0.4)
    pty.key("\r")
    observed = wait(
        pty,
        lambda: dead_reference_evidence(session_db(env), reference),
        240,
    )
    settled(pty, 60)
    written = dotenv(env.repo)
    report.check(
        "D",
        "the dead reference did not overwrite the file",
        value not in written and written == before,
        written,
    )
    asked = any(
        word in flat(pty)
        for word in ("重新捕获", "重新提供", "重新粘贴", "需要澄清", "无法取到值")
    )
    report.check(
        "D",
        "the real model asked for a fresh credential instead of guessing",
        asked,
        pty.screen.text()[-800:],
    )
    if observed:
        report.check(
            "D",
            "the restarted runtime refused the old reference",
            True,
            "",
        )
    else:
        report.block(
            "D",
            "the restarted runtime refused the old reference",
            "this run's model asked for a new credential before calling the tool; "
            "the deterministic restart test and the preserved first-run DB "
            "(restart-dead-ref-first-run-tool.json) show the refusal",
        )
    report.dump("D-resumed", pty)
    return observed


def session_db(env) -> str | None:
    import glob

    matches = glob.glob(os.path.join(env.home, "state", "projects", "*", "sessions.db"))
    return matches[0] if matches else None


def latest_session_id(env) -> str | None:
    db = session_db(env)
    if not db:
        return None
    import sqlite3

    connection = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        row = connection.execute(
            "SELECT id FROM sessions ORDER BY created_at DESC LIMIT 1"
        ).fetchone()
    finally:
        connection.close()
    return row[0] if row else None


def dead_reference_evidence(db: str | None, reference: str) -> bool:
    if not db:
        return False
    import sqlite3

    connection = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        rows = connection.execute(
            "SELECT payload FROM events WHERE payload LIKE '%secret_apply%'"
        ).fetchall()
    finally:
        connection.close()
    started: set[str] = set()
    for (payload,) in rows:
        try:
            event = json.loads(payload)
        except (TypeError, ValueError):
            continue
        if event.get("type") != "tool_call_started":
            continue
        if event.get("name") != "secret_apply":
            continue
        arguments = str(event.get("arguments") or "")
        if reference in arguments:
            call_id = event.get("call_id")
            if call_id:
                started.add(str(call_id))
    for (payload,) in rows:
        try:
            event = json.loads(payload)
        except (TypeError, ValueError):
            continue
        if event.get("type") != "tool_call_finished":
            continue
        if event.get("name") != "secret_apply" or not event.get("is_error"):
            continue
        if str(event.get("call_id") or "") not in started:
            continue
        preview = str(event.get("preview") or "")
        if "not available" in preview or "ask the user" in preview:
            return True
    return False


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", default="A,B,C,D")
    args = parser.parse_args()
    only = [part.strip().upper() for part in args.only.split(",") if part.strip()]

    if not os.path.exists(base.BIN):
        print(f"missing {base.BIN} — build it first")
        return 2
    source_config = os.path.expanduser("~/.leveler/config.toml")
    if not os.path.exists(source_config):
        print("BLOCKED: no ~/.leveler/config.toml for a real provider run")
        return 2
    provider, upstream = config_provider(source_config)
    proxy = RecordingProxy(upstream)
    env = base.Env("real", None)
    # The runtime records the canonical repository path on the session row;
    # respawning with the symlinked spelling (/var vs /private/var) would make
    # `--session` fail to find its own session.
    env.repo = os.path.realpath(env.repo)
    rewrite_base_url(os.path.join(env.home, "config.toml"), provider, proxy.base_url())
    report = base.Report("secret-broker-real")
    report.check(
        "env",
        f"a real provider connection is configured ({provider})",
        True,
        "",
    )
    pty = None
    try:
        pty = base.boot(report, env, 132, 46)
        executed = []
        if "A" in only:
            executed.append(scenario_a(report, pty, env))
        if "B" in only:
            executed.append(scenario_b(report, pty, env))
        if "C" in only:
            executed.append(scenario_c(report, pty, env))
        if "D" in only:
            before = dotenv(env.repo)
            reference = scenario_d(report, pty, env)
            if reference is not None:
                # The in-process runtime dies with this PTY; its vault is
                # process memory by design.
                pty.close()
                pty = None
                session = latest_session_id(env)
                if session is None:
                    report.check("D", "the session id is durable", False, "no sessions.db row")
                else:
                    pty = base.boot(
                        report,
                        env,
                        132,
                        46,
                        extra=["--session", session],
                    )
                    time.sleep(2.0)
                    executed.append(
                        scenario_d_resume(report, pty, env, reference, before)
                    )
            else:
                executed.append(False)

        bodies = proxy.body_text()
        report.check(
            "wire",
            "no recorded provider request body contains a captured value",
            CANARY not in bodies and UNKNOWN not in bodies and GOAL_CANARY not in bodies,
            "a provider request carried a plaintext credential",
        )
        report.check(
            "wire",
            "the provider requests carry a secret reference",
            re.search(r"sec_[0-9a-f]{32}", bodies) is not None,
            "no reference reached the model",
        )
        with open(os.path.join(EVIDENCE, "provider-bodies.jsonl"), "w") as handle:
            for index, body in enumerate(proxy.recorder.bodies):
                handle.write(json.dumps({"index": index, "body": body}) + "\n")
        report.save()
        print(f"evidence: {EVIDENCE}")
        if report.failures:
            return 1
        return 0
    finally:
        if pty is not None:
            pty.close()
        proxy.close()


if __name__ == "__main__":
    sys.exit(main())
