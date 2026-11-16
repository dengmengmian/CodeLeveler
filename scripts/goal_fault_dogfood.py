#!/usr/bin/env python3
"""Real-model fault injection on the REAL provider wire.

The model is the machine's configured provider (deepseek-v4.1-flash-1017 by
default). A local forwarding proxy sits between the runtime and that provider.
It forwards every request byte-for-byte, except one response: once the model
has begun emitting a tool call, the proxy truncates the SSE stream and closes
the client connection. That is exactly the original failure shape — reasoning
and partial tool-call arguments received, no tool dispatched, the stream cut.

What this proves (the runtime, PTY and store are the real ones):

  * the model-round lifecycle re-requests the SAME logical request after the
    cut instead of ending the task (the failed attempt's tool call is never
    dispatched, so the retry is safe);
  * the retried round really executes the tool once — no duplicate write;
  * the user never types `继续`;
  * the TUI ends on a real completion, with no failure card and no goal-control
    row.

Evidence is written to target/goal-fault-dogfood/.
Usage: python3 scripts/goal_fault_dogfood.py [--provider real]
"""

from __future__ import annotations

import argparse
import glob
import http.client
import json
import os
import re
import sqlite3
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import pty_dogfood as H  # noqa: E402

EVIDENCE = os.path.join(H.ROOT, "target", "goal-fault-dogfood")
H.EVIDENCE = EVIDENCE

PROMPT = (
    "创建 recover-probe.txt，内容正好是一行 `ok`。只创建这个文件，随后告诉我完成。"
)


class Proxy(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, upstream: str):
        parsed = urlparse(upstream)
        self.upstream_host = parsed.hostname
        self.upstream_port = parsed.port or 443
        self.upstream_path = parsed.path.rstrip("/")
        self.https = parsed.scheme == "https"
        self.fault_armed = True
        self.fault_mode = "stream"
        self.cuts = 0
        self.goal_faults = 0
        self.agent_requests = 0
        self.requests = 0
        # Optional barrier for a harness that needs the second agent request
        # (the recovery window) held open while it drives real UI actions.
        self.held = threading.Event()
        self.hold_release: threading.Event | None = None
        self.request_bodies: list[bytes] = []
        super().__init__(("127.0.0.1", 0), Handler)
        self.port = self.server_address[1]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # noqa: D401 - keep the harness quiet
        return

    def do_POST(self):  # noqa: N802 - BaseHTTPRequestHandler API
        proxy: Proxy = self.server
        length = int(self.headers.get("Content-Length", "0") or 0)
        body = self.rfile.read(length)
        proxy.requests += 1
        proxy.request_bodies.append(body)

        if b'"tools"' in body:
            proxy.agent_requests += 1
            if proxy.agent_requests == 2 and proxy.hold_release is not None:
                proxy.held.set()
                proxy.hold_release.wait(180)
        if (
            proxy.fault_armed
            and proxy.fault_mode == "goal"
            and b'"tools"' in body
        ):
            # A complete provider answer, not a transport cut: the model round
            # has nothing to retry (the provider responded), so only the GOAL
            # lifecycle can continue the task. This is the Goal-level fault a
            # request-level retry must not be confused with.
            proxy.fault_armed = False
            proxy.goal_faults += 1
            data = json.dumps({"error": {"message": "gateway timeout"}}).encode()
            self.send_response(504)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                self.wfile.write(data)
            except Exception:
                pass
            return

        headers = {
            key: value
            for key, value in self.headers.items()
            if key.lower()
            not in ("host", "content-length", "connection", "accept-encoding")
        }
        path = self.path
        if proxy.upstream_path and not path.startswith(proxy.upstream_path):
            path = proxy.upstream_path + path
        conn_cls = http.client.HTTPSConnection if proxy.https else http.client.HTTPConnection
        conn = conn_cls(proxy.upstream_host, proxy.upstream_port, timeout=600)
        conn.request("POST", path, body=body, headers=headers)
        upstream = conn.getresponse()

        self.send_response(upstream.status)
        for key, value in upstream.getheaders():
            if key.lower() in ("transfer-encoding", "content-length", "connection"):
                continue
            self.send_header(key, value)
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        fault = proxy.fault_armed
        tool_seen = False
        try:
            while True:
                chunk = upstream.read(2048)
                if not chunk:
                    break
                self.wfile.write(b"%x\r\n" % len(chunk) + chunk + b"\r\n")
                self.wfile.flush()
                tool_seen = tool_seen or b'"tool_calls"' in chunk
                if fault and tool_seen:
                    # The model has begun a tool call and no tool from this
                    # attempt can have been dispatched (the round never
                    # returned). Truncate: the client sees a stream cut with
                    # partial tool arguments, exactly the real failure.
                    proxy.fault_armed = False
                    proxy.cuts += 1
                    self.close_connection = True
                    try:
                        self.connection.shutdown(2)
                    except OSError:
                        pass
                    self.connection.close()
                    return
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        finally:
            try:
                upstream.close()
            except Exception:
                pass
            try:
                conn.close()
            except Exception:
                pass


def patch_config(home: str, port: int):
    path = os.path.join(home, "config.toml")
    text = open(path).read()
    pattern = re.compile(r'(base_url\s*=\s*")https://taotoken\.net/api/v1(")')
    patched, count = pattern.subn(rf"\g<1>http://127.0.0.1:{port}/api/v1\g<2>", text)
    if count == 0:
        # Fall back to the first provider base_url the machine config names.
        patched, count = re.subn(
            r'(base_url\s*=\s*")https://[^"]+(")', rf"\g<1>http://127.0.0.1:{port}/api/v1\g<2>", text, count=1
        )
    if count != 1:
        raise SystemExit("cannot point the provider at the fault proxy")
    open(path, "w").write(patched)


def rows(home: str, sql: str):
    found = []
    for path in glob.glob(os.path.join(home, "**", "*.db"), recursive=True):
        try:
            conn = sqlite3.connect("file:%s?mode=ro" % path, uri=True)
            conn.row_factory = sqlite3.Row
            found.extend(dict(row) for row in conn.execute(sql))
            conn.close()
        except sqlite3.Error:
            continue
    return found


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", choices=["real"], default="real")
    parser.add_argument(
        "--fault",
        choices=["stream", "goal"],
        default="stream",
        help="stream: cut an SSE tool-argument stream (request-level recovery). "
        "goal: answer the first agent request with HTTP 504 (Goal-level recovery).",
    )
    args = parser.parse_args()
    os.makedirs(EVIDENCE, exist_ok=True)

    source = os.path.expanduser("~/.leveler/config.toml")
    upstream = re.search(r'base_url\s*=\s*"(https://[^"]+)"', open(source).read())
    if upstream is None:
        raise SystemExit("the machine config names no provider base_url")
    proxy = Proxy(upstream.group(1))
    proxy.fault_mode = args.fault
    threading.Thread(target=proxy.serve_forever, daemon=True).start()

    env = H.Env("real", None)
    home, repo = env.home, env.repo
    with open(os.path.join(repo, "verify.sh"), "w") as fh:
        fh.write("#!/bin/sh\necho VERIFY-OK\nexit 0\n")
    patch_config(home, proxy.port)

    report = H.Report("goal-fault-real" if args.fault == "stream" else "goal-fault-real-goal")
    pty_ = H.boot(report, env, 120, 44)
    if args.fault == "goal":
        PROMPT_TEXT = "/goal " + PROMPT
    else:
        PROMPT_TEXT = PROMPT
    try:
        pty_.write(PROMPT_TEXT)
        time.sleep(0.2)
        pty_.key("\r")
        recovery_status = False
        if args.fault == "goal":
            recovery_status = H.wait_until(
                pty_, lambda text: "正在恢复任务" in text, timeout=90
            )
        done = H.wait_until(
            pty_,
            lambda text: "任务已完成" in text or "目标已完成" in text or "已完成" in text,
            timeout=600,
        )
        pty_.wait_stable(2.0, 60)
        report.dump("fault-model-round", pty_)
        screen = pty_.screen.text()
        if args.fault == "goal":
            attempts = rows(
                home,
                "SELECT turn_id, error_kind, retry_count FROM model_requests "
                "WHERE turn_id IS NOT NULL",
            )
            report.check(
                "goal-fault",
                "the runtime opened a Goal recovery window after the 504",
                proxy.goal_faults == 1,
                str(proxy.goal_faults),
            )
            report.check(
                "goal-fault",
                "the 504 was one provider attempt with NO request-level retry",
                sum(
                    1
                    for row in attempts
                    if row["error_kind"] == "provider_unavailable"
                )
                == 1
                and all(
                    row["retry_count"] == 0
                    for row in attempts
                    if row["error_kind"] == "provider_unavailable"
                ),
                repr(attempts[:8]),
            )
            report.check(
                "goal-fault",
                "the recovery status is shown while the continuation runs",
                recovery_status,
                pty_.screen.text()[-600:],
            )
            turns = rows(home, "SELECT id, payload FROM turns ORDER BY ordinal")
            report.check(
                "goal-fault",
                "the recovery is a continuation of the same goal, not a new task",
                len(turns) >= 2
                and all(
                    (json.loads(row["payload"] or "{}").get("goal_id")
                     == json.loads(turns[0]["payload"] or "{}").get("goal_id"))
                    for row in turns
                ),
                repr([row["id"] for row in turns]),
            )
            report.check(
                "goal-fault",
                "the task completed without the user typing 继续",
                done and os.path.exists(os.path.join(repo, "recover-probe.txt")),
                screen[-900:],
            )
            user_inputs = rows(
                home,
                "SELECT count(*) AS n FROM session_messages WHERE payload LIKE '%user_input%'",
            )
            report.check(
                "goal-fault",
                "the user sent one message only (no manual continue)",
                bool(user_inputs) and user_inputs[0]["n"] == 1,
                repr(user_inputs),
            )
            report.check(
                "goal-fault",
                "no failure card survived the recovery",
                "请求失败" not in screen,
                screen[-900:],
            )
            report.check(
                "goal-fault",
                "no goal-control row reached the conversation",
                "目标收尾" not in screen and "目标未更新" not in screen,
                screen[-900:],
            )
            report.save()
            print("assertions: %d, failures: %d" % (len(report.results), report.failures))
            return 0 if report.failures == 0 else 1
        report.check("fault", "the provider stream really was truncated", proxy.cuts == 1, str(proxy.cuts))
        report.check(
            "fault",
            "the runtime retried the same logical request",
            any(row.get("retry_count", 0) >= 1 for row in rows(home, "SELECT retry_count FROM model_requests")),
            repr(rows(home, "SELECT retry_count, error_kind FROM model_requests")),
        )
        report.check(
            "fault",
            "the task completed without the user typing 继续",
            done and os.path.exists(os.path.join(repo, "recover-probe.txt")),
            screen[-900:],
        )
        writes = rows(
            home,
            "SELECT count(*) AS n FROM events WHERE type='tool_call_started' AND payload LIKE '%write_file%'",
        )
        report.check(
            "fault",
            "the failed attempt's tool call was not dispatched; the retry wrote once",
            bool(writes) and writes[0]["n"] == 1,
            repr(writes),
        )
        report.check(
            "fault",
            "no failure card survived the recovery",
            "请求失败" not in screen,
            screen[-900:],
        )
        report.check(
            "fault",
            "no goal-control row reached the conversation",
            "目标收尾" not in screen and "目标未更新" not in screen,
            screen[-900:],
        )
        report.check(
            "fault",
            "the user sent one message only (no manual continue)",
            bool(rows(home, "SELECT count(*) AS n FROM session_messages WHERE payload LIKE '%user_input%'"))
            and rows(home, "SELECT count(*) AS n FROM session_messages WHERE payload LIKE '%user_input%'")[0]["n"] == 1,
            repr(rows(home, "SELECT count(*) AS n FROM session_messages WHERE payload LIKE '%user_input%'")),
        )
    finally:
        pty_.close()
        proxy.shutdown()
    report.save()
    print("assertions: %d, failures: %d" % (len(report.results), report.failures))
    return 0 if report.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
