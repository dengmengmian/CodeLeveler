#!/usr/bin/env python3
"""Real-PTY + real-provider dogfood for capability exposure across resume.

What this proves, and from which bytes:

  * the REAL `leveler` binary on a REAL pseudo-terminal, with the machine's own
    DeepSeek provider config, and
  * the EXACT request bodies that left the provider adapter, captured by a
    recording HTTP proxy this script sits in front of the upstream gateway.

No mock: `--provider real` contacts the configured DeepSeek gateway. The proxy
records request bodies only (never the `Authorization` header), and the screen
model captures the TUI's own frames.

Scenarios (select with --phase, default: all):

  DF-01  fresh Chat session: enable multi_agent, then a delegation turn.
  DF-02  normal exit (Ctrl+C twice), reopen with --session, enable, next turn.
  DF-03  SIGKILL between turns, reopen with --session, enable, next turn.

The product property asserted is exposure, never the model's choice:

  request N carries `capability` enable -> request N+1 lists `spawn_agent`,
  and the turn after that still does.

Whether the model actually delegates is reported separately (called / not
called / child result), because autonomy is the model's, not the harness's.

Usage:
    source ~/.leveler/deepseek.env      # or export DEEPSEEK_API_KEY yourself
    cargo build -p leveler-cli --bin leveler
    python3 scripts/capability_resume_pty.py --phase all

Evidence: target/capability-resume-dogfood/<stamp>/
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import signal
import sqlite3
import ssl
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_BIN = os.path.join(ROOT, "target", "debug", "leveler")
STAMP = time.strftime("%Y%m%d-%H%M%S")
EVIDENCE = os.path.join(ROOT, "target", "capability-resume-dogfood", STAMP)
UPSTREAM_HOST = "taotoken.net"


def load_pty_module():
    path = os.path.join(ROOT, "scripts", "pty_dogfood.py")
    spec = importlib.util.spec_from_file_location("pty_dogfood", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# ---------------------------------------------------------------------------
# Recording proxy: the only place the real request body is read
# ---------------------------------------------------------------------------


class RecordingProxy:
    def __init__(self, evidence_dir: str):
        self.path = os.path.join(evidence_dir, "requests.jsonl")
        self.lock = threading.Lock()
        self.tag = {"phase": "boot", "turn": 0}
        self.seq = 0
        self._fh = open(self.path, "w")
        proxy = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):  # silence
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length)
                proxy.record(self.path, body)
                headers = {
                    key: value
                    for key, value in self.headers.items()
                    if key.lower() not in ("host", "connection", "content-length")
                }
                conn = ssl.create_default_context().wrap_socket(
                    __import__("socket").create_connection((UPSTREAM_HOST, 443), timeout=60),
                    server_hostname=UPSTREAM_HOST,
                )
                conn.settimeout(600)
                request = [f"POST {self.path} HTTP/1.1", f"Host: {UPSTREAM_HOST}"]
                for key, value in headers.items():
                    request.append(f"{key}: {value}")
                request.append("Connection: close")
                request.append("Content-Length: " + str(len(body)))
                payload = ("\r\n".join(request) + "\r\n\r\n").encode() + body
                conn.sendall(payload)
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                try:
                    while True:
                        chunk = conn.recv(1 << 16)
                        if not chunk:
                            break
                        self.wfile.write(chunk)
                        self.wfile.flush()
                except Exception as error:  # keep the body we already relayed
                    proxy.note_stream_error(str(error))
                finally:
                    conn.close()
                self.close_connection = True

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def base_url(self) -> str:
        return "http://127.0.0.1:%d/api/v1" % self.server.server_address[1]

    def note_stream_error(self, message: str):
        with self.lock:
            self.seq += 1
            self._fh.write(json.dumps({"kind": "stream_error", "error": message}) + "\n")
            self._fh.flush()

    def record(self, path: str, body: bytes):
        try:
            request = json.loads(body.decode("utf-8", "replace"))
        except Exception:
            request = {}
        tool_names = [
            (tool.get("function") or {}).get("name") or tool.get("name")
            for tool in (request.get("tools") or [])
        ]
        messages = request.get("messages") or []
        calls = []
        results = {}
        text = []
        for message in messages:
            if message.get("role") == "assistant":
                for call in message.get("tool_calls") or []:
                    calls.append((call.get("function") or {}).get("name"))
                    text.append((call.get("function") or {}).get("arguments") or "")
            if message.get("role") == "tool":
                content = message.get("content") or ""
                results[message.get("tool_call_id") or ""] = content[:400]
                text.append(content)
            if message.get("role") == "user" and isinstance(message.get("content"), str):
                text.append(message["content"])
        blob = "\n".join(text)
        enable_seen = "enabled; tools available" in blob
        enable_count = blob.count("enabled; tools available on the following request")
        exposed_values = {}
        parse_failure = None
        for message in messages:
            content = message.get("content")
            if not isinstance(content, str) or '"exposed"' not in content:
                continue
            try:
                entries = json.loads(content)
            except Exception as error:
                # The capability catalog may be wrapped in prose; find the list.
                start, end = content.find("["), content.rfind("]")
                try:
                    entries = json.loads(content[start : end + 1])
                except Exception:
                    parse_failure = str(error)
                    continue
            for entry in entries if isinstance(entries, list) else []:
                if isinstance(entry, dict) and isinstance(entry.get("id"), str):
                    exposed_values[entry["id"]] = entry.get("exposed")
        with self.lock:
            self.seq += 1
            record = {
                "kind": "request",
                "seq": self.seq,
                "phase": dict(self.tag),
                "path": path,
                "model": request.get("model"),
                "tool_names": tool_names,
                "tool_count": len(tool_names),
                "spawn_agent": "spawn_agent" in tool_names,
                "calls": calls,
                "messages": len(messages),
                "enable_result_seen": enable_seen,
                "enable_result_count": enable_count,
                "exposed": exposed_values,
                "catalog_parse_error": parse_failure,
                "tool_results_tail": list(results.values())[-4:],
            }
            self._fh.write(json.dumps(record, ensure_ascii=False) + "\n")
            self._fh.flush()

    def close(self):
        self.server.shutdown()
        self._fh.close()


# ---------------------------------------------------------------------------
# Environment: isolated HOME, isolated repo, real provider via the proxy
# ---------------------------------------------------------------------------


def deepseek_key() -> str:
    key = os.environ.get("DEEPSEEK_API_KEY")
    if key:
        return key
    env_file = os.path.expanduser("~/.leveler/deepseek.env")
    if os.path.exists(env_file):
        for line in open(env_file):
            line = line.strip()
            if line.startswith("export DEEPSEEK_API_KEY="):
                return line.split("=", 1)[1].strip().strip("'\"")
    raise SystemExit("DEEPSEEK_API_KEY is not set and ~/.leveler/deepseek.env is unreadable")


REPO_FILES = {
    "inventory.py": "def restock(item, qty):\n    return qty * 2\n",
    "billing.py": "def total(lines):\n    return sum(lines)\n",
    "shipping.py": "def eta(days):\n    return days + 1\n",
    "audit.py": "def record(event):\n    return event\n",
}


def build_workspace(proxy_url: str, key: str):
    root = tempfile.mkdtemp(prefix="leveler-cap-resume-")
    home = os.path.join(root, "home")
    repo = os.path.join(root, "repo")
    os.makedirs(home)
    os.makedirs(os.path.join(repo, ".leveler"))
    os.makedirs(os.path.join(repo, "src"))
    with open(os.path.join(repo, ".leveler", "config.yaml"), "w") as fh:
        fh.write("{}\n")
    for name, body in REPO_FILES.items():
        with open(os.path.join(repo, "src", name), "w") as fh:
            fh.write(body)
    subprocess.run(["git", "init", "-q"], cwd=repo, check=False)
    subprocess.run(
        ["git", "-c", "user.email=a@b", "-c", "user.name=t", "add", "-A"], cwd=repo, check=False
    )
    subprocess.run(
        [
            "git",
            "-c",
            "user.email=a@b",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
        cwd=repo,
        check=False,
    )
    with open(os.path.join(home, "config.toml"), "w") as fh:
        fh.write(
            'default_model = "deepseek/deepseek-flash"\nlang = "zh"\n\n'
            "[providers.deepseek]\n"
            'protocol = "openai_chat"\n'
            f'base_url = "{proxy_url}"\n'
            'api_key_env = "DEEPSEEK_API_KEY"\n'
            'timeouts = { connect_seconds = 20, request_seconds = 600, idle_stream_seconds = 180 }\n\n'
            '[models."deepseek-flash"]\n'
            'provider = "deepseek"\n'
            'model_id = "deepseek-flash"\n'
            'protocol = "openai_chat"\n'
            "context_window = 1048576\n"
            "reliable_context = 786432\n"
            "max_output_tokens = 65536\n"
            "parallel_tool_calls = true\n"
            "max_parallel_tool_calls = 1\n"
            "reasoning = true\n"
            'reasoning_style = "thinking_flag"\n'
            'supported_efforts = ["low", "high"]\n'
            'reasoning_effort = "low"\n'
            "thinking_supports_forced_tool_choice = false\n"
            "passback_reasoning_content = true\n"
        )
    env = dict(os.environ)
    env.update(
        {
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "LEVELER_HOME": home,
            "LEVELER_LOG": "info",
            "DEEPSEEK_API_KEY": key,
        }
    )
    return root, home, repo, env


def session_database(home: str):
    for base, _dirs, files in os.walk(os.path.join(home, "state")):
        if "sessions.db" in files:
            return os.path.join(base, "sessions.db")
    return None


def latest_session(home: str):
    db = session_database(home)
    if not db:
        return None
    try:
        conn = sqlite3.connect(db)
        rows = conn.execute(
            "SELECT id, collaboration, model FROM sessions ORDER BY created_at DESC LIMIT 1"
        ).fetchall()
        conn.close()
        return rows[0] if rows else None
    except Exception:
        return None


def is_parent(record) -> bool:
    """A top-level turn's own model call.

    Children get `report_finding` and never `commit_task`; the top-level parent
    is the other way round. This is how the evidence separates a parent request
    from a sub-agent request without trusting order.
    """
    names = record.get("tool_names") or []
    return "commit_task" in names and "report_finding" not in names


def read_requests(proxy: RecordingProxy):
    if not os.path.exists(proxy.path):
        return []
    records = []
    for line in open(proxy.path):
        record = json.loads(line)
        if record.get("kind") == "request":
            records.append(record)
    return records


# ---------------------------------------------------------------------------
# PTY driving
# ---------------------------------------------------------------------------


class Dogfood:
    def __init__(self, phase: str):
        self.phase = phase
        self.pty_module = load_pty_module()
        self.proxy = RecordingProxy(EVIDENCE)
        self.root, self.home, self.repo, self.env = build_workspace(
            self.proxy.base_url(), deepseek_key()
        )
        self.checks = []
        self.notes = []
        self.pty = None
        self.log = open(os.path.join(EVIDENCE, "steps.log"), "w")

    # -- reporting ---------------------------------------------------------
    def note(self, text: str):
        print(text)
        self.log.write(text + "\n")
        self.log.flush()

    def check(self, name: str, ok: bool, detail: str = ""):
        self.checks.append({"name": name, "ok": bool(ok), "detail": detail})
        self.note(("  PASS  " if ok else "  FAIL  ") + name + ((" — " + detail) if detail else ""))
        return ok

    def dump(self, label: str, pty_):
        path = os.path.join(EVIDENCE, "screen-%s.txt" % label)
        with open(path, "w") as fh:
            fh.write(pty_.screen.text())
        return path

    # -- process -----------------------------------------------------------
    def spawn(self, session: str | None = None):
        argv = [
            BIN,
            "--repo",
            self.repo,
            "tui",
            "--auto-approve",
            "--in-process",
            "--permission",
            "full-access",
        ]
        if session:
            argv += ["--session", session]
        self.note("spawn: " + " ".join(argv))
        pty_ = self.pty_module.Pty(argv, self.env, 120, 48)
        pty_.wait_for("leveler", timeout=60) or pty_.wait_stable(1.5, 60)
        self.pty = pty_
        return pty_

    def turn(self, text: str, label: str = "", timeout: float = 420):
        pty_ = self.pty
        self._turn = getattr(self, "_turn", 0) + 1
        self.proxy.tag = {"phase": self.phase, "turn": self._turn, "label": label}
        before = len(read_requests(self.proxy))
        self.note("user: " + text)
        pty_.write(text)
        time.sleep(0.4)
        pty_.key("\r")
        deadline = time.time() + timeout
        last_activity = time.time()
        seen = before
        while time.time() < deadline:
            time.sleep(0.5)
            count = len(read_requests(self.proxy))
            if count != seen:
                seen = count
                last_activity = time.time()
            elif count > before and time.time() - last_activity > 4.0:
                break
            if not pty_.alive():
                break
        pty_.wait_stable(2.0, 60)
        requests = read_requests(self.proxy)[before:]
        self.note("  provider requests this turn: %d" % len(requests))
        return requests

    def quit_gracefully(self):
        pty_ = self.pty
        # Idle Ctrl+C arms the quit prompt; the second one quits.
        pty_.key("\x03")
        time.sleep(0.6)
        pty_.key("\x03")
        deadline = time.time() + 30
        while time.time() < deadline and pty_.alive():
            time.sleep(0.2)
        pty_.close()

    def sigkill(self):
        pty_ = self.pty
        self.note("SIGKILL pid %d" % pty_.proc.pid)
        os.killpg(os.getpgid(pty_.proc.pid), signal.SIGKILL)
        time.sleep(1.0)
        pty_.close()

    # -- evidence ----------------------------------------------------------
    def write_evidence(self):
        requests = read_requests(self.proxy)
        spawn_calls = [r for r in requests if "spawn_agent" in (r["calls"] or [])]
        child_results = [
            value
            for r in requests
            for value in (r["tool_results_tail"] or [])
            if "spawn" in value.lower() or "子代理" in value
        ]
        evidence = {
            "stamp": STAMP,
            "phase": self.phase,
            "binary": BIN,
            "provider": {
                "protocol": "openai_chat",
                "upstream": UPSTREAM_HOST,
                "model": "deepseek/deepseek-flash",
                "captured_by": "recording HTTP proxy (request bodies; no auth headers)",
            },
            "workspace": self.repo,
            "session": latest_session(self.home),
            "requests": requests,
            "spawn_agent_calls": spawn_calls,
            "child_result_samples": child_results[:3],
            "checks": self.checks,
            "notes": self.notes,
            "git": git_state(),
        }
        with open(os.path.join(EVIDENCE, "evidence.json"), "w") as fh:
            json.dump(evidence, fh, ensure_ascii=False, indent=2)
        self.note("evidence: " + EVIDENCE)

    def close(self):
        try:
            if self.pty and self.pty.alive():
                self.pty.close()
        finally:
            self.proxy.close()
            self.log.close()


def git_state():
    def run(*args):
        result = subprocess.run(args, cwd=ROOT, capture_output=True, text=True)
        return result.stdout.strip()

    return {
        "commit": run("git", "rev-parse", "HEAD"),
        "dirty_files": len(run("git", "status", "--porcelain").splitlines()),
        "branch": run("git", "rev-parse", "--abbrev-ref", "HEAD"),
    }


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------

ENABLE = (
    "请先启用 multi_agent 能力：调用 capability 工具，action=enable，id=multi_agent。"
    "启用后只回复结果，不要做别的。"
)
STATUS = (
    "不要做别的：只调用 capability 工具，action=status，"
    "然后把 multi_agent 的 exposed 值原样告诉我。"
)
DELEGATE = (
    "现在请用子代理并行检查 src/ 下的 inventory.py、billing.py、shipping.py："
    "每个子代理只看一个文件并报告潜在 bug，最后汇总。"
)


def parents(records):
    """The top-level turn's own model calls, in order."""
    return [record for record in records if is_parent(record)]


def new_enable(records):
    """The request that first carried each new enable result, in order.

    The count is taken from the request body itself, so history replay cannot
    satisfy it: only a request whose tool results contain one MORE enable
    confirmation than the previous parent request did counts as new.
    """
    parents_in_turn = parents(records)
    if not parents_in_turn:
        return []
    # The turn's first parent request replays whatever the transcript already
    # holds, including an enable from an earlier turn or an earlier process.
    # Only a count that grows past that baseline is a new enable.
    seen = parents_in_turn[0].get("enable_result_count", 0)
    marks = []
    for record in parents_in_turn[1:]:
        count = record.get("enable_result_count", 0)
        if count > seen:
            marks.append(record)
            seen = count
    return marks


def report_model_choice(d: Dogfood, label: str, requests):
    called = [
        record
        for record in parents(requests)
        if "spawn_agent" in (record.get("calls") or [])
    ]
    if called:
        d.note("  model did delegate: spawn_agent requested in request(s) %s"
               % ", ".join(str(r["seq"]) for r in called))
    else:
        d.note("  model did not delegate in this turn (its own choice; exposure is measured above)")
    return called


def df01_fresh(d: Dogfood):
    d.note("== DF-01 fresh Chat session ==")
    d.spawn()
    enabled_turn = d.turn(ENABLE, "df01-enable")
    marks = new_enable(enabled_turn)
    d.check(
        "DF-01 the capability enable result reached the model",
        bool(marks),
        "parent requests=%d" % len(parents(enabled_turn)),
    )
    for mark in marks:
        d.check(
            "DF-01 request %d carries the enable result and lists spawn_agent" % mark["seq"],
            mark["spawn_agent"],
            "tools=%d" % mark["tool_count"],
        )
    delegation_turn = d.turn(DELEGATE, "df01-delegate")
    first = parents(delegation_turn)[:1]
    if first:
        d.check(
            "DF-01 the next turn's first parent request still lists spawn_agent",
            first[0]["spawn_agent"],
            "seq=%d tools=%d" % (first[0]["seq"], first[0]["tool_count"]),
        )
        report_model_choice(d, "DF-01", delegation_turn)
    else:
        d.check("DF-01 the delegation turn reached the model", False)
    d.dump("df01", d.pty)
    return d


def df04_status(d: Dogfood):
    """The state the MODEL is told, read from its own tool result on the wire."""
    d.note("== DF-04 capability status after the enable ==")
    records = d.turn(STATUS, "df04-status")
    parents_in_turn = parents(records)
    d.check("DF-04 the status call reached the model", len(parents_in_turn) >= 2)
    if len(parents_in_turn) >= 2:
        last = parents_in_turn[-1]
        d.check(
            "DF-04 the catalog the model reads says multi_agent exposed",
            last["exposed"].get("multi_agent") is True,
            "exposed=%s" % last["exposed"].get("multi_agent"),
        )
        d.check(
            "DF-04 the same request lists spawn_agent",
            last["spawn_agent"],
            "seq=%d tools=%d" % (last["seq"], last["tool_count"]),
        )
    d.dump("df04", d.pty)
    return d


def df02_normal_resume(d: Dogfood):
    d.note("== DF-02 normal exit, then --session ==")
    # DF-02 continues from DF-01's session.
    d.quit_gracefully()
    session = latest_session(d.home)
    d.check("DF-02 the session was persisted", session is not None, str(session))
    if not session:
        return d
    d.spawn(session=session[0])
    d.note("  resumed session %s (axis=%s, model=%s)" % session)
    resumed_first = d.turn(ENABLE, "df02-enable")
    first = parents(resumed_first)[:1]
    if first:
        d.check(
            "DF-02 the first parent request after resume carries no dead process's tool",
            not first[0]["spawn_agent"],
            "seq=%d spawn_agent=%s" % (first[0]["seq"], first[0]["spawn_agent"]),
        )
    marks = new_enable(resumed_first)
    d.check("DF-02 enable in the resumed session reached the model", bool(marks))
    for mark in marks:
        d.check(
            "DF-02 request %d carries the enable result and lists spawn_agent" % mark["seq"],
            mark["spawn_agent"],
            "tools=%d exposed=%s" % (mark["tool_count"], mark["exposed"].get("multi_agent")),
        )
    delegation_turn = d.turn(DELEGATE, "df02-delegate")
    first = parents(delegation_turn)[:1]
    if first:
        d.check(
            "DF-02 the resumed session's next turn still lists spawn_agent",
            first[0]["spawn_agent"],
            "seq=%d" % first[0]["seq"],
        )
        report_model_choice(d, "DF-02", delegation_turn)
    d.dump("df02", d.pty)
    return d


def df03_sigkill_resume(d: Dogfood):
    d.note("== DF-03 SIGKILL, then --session ==")
    d.sigkill()
    session = latest_session(d.home)
    d.check("DF-03 the session survived the kill on disk", session is not None, str(session))
    if not session:
        return d
    d.spawn(session=session[0])
    d.note("  resumed session %s after SIGKILL" % (session[0],))
    requests = d.turn(ENABLE, "df03-enable")
    first = parents(requests)[:1]
    if first:
        d.check(
            "DF-03 the killed process's in-memory activation is gone from the first parent request",
            not first[0]["spawn_agent"],
            "seq=%d" % first[0]["seq"],
        )
    marks = new_enable(requests)
    d.check("DF-03 enable after SIGKILL reached the model", bool(marks))
    for mark in marks:
        d.check(
            "DF-03 request %d carries the enable result and lists spawn_agent" % mark["seq"],
            mark["spawn_agent"],
            "tools=%d" % mark["tool_count"],
        )
    delegation_turn = d.turn(DELEGATE, "df03-delegate")
    first = parents(delegation_turn)[:1]
    if first:
        d.check(
            "DF-03 the next turn after SIGKILL resume still lists spawn_agent",
            first[0]["spawn_agent"],
            "seq=%d" % first[0]["seq"],
        )
        report_model_choice(d, "DF-03", delegation_turn)
    d.dump("df03", d.pty)
    return d


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--phase", default="all", choices=["all", "df01", "df02", "df03", "df04"]
    )
    parser.add_argument(
        "--binary",
        default=DEFAULT_BIN,
        help="leveler binary under test (default: target/debug/leveler)",
    )
    parser.add_argument(
        "--tag",
        default="",
        help="suffix for the evidence directory, e.g. `before` / `after`",
    )
    args = parser.parse_args()
    global BIN, EVIDENCE
    BIN = os.path.abspath(args.binary)
    if args.tag:
        EVIDENCE = EVIDENCE + "-" + args.tag
    os.makedirs(EVIDENCE, exist_ok=True)
    d = Dogfood(args.phase)
    try:
        d.note("evidence dir: " + EVIDENCE)
        d.note("binary: " + BIN)
        if args.phase in ("all", "df01"):
            df01_fresh(d)
        if args.phase in ("all", "df04"):
            df04_status(d)
        if args.phase in ("all", "df02"):
            df02_normal_resume(d)
        if args.phase in ("all", "df03"):
            df03_sigkill_resume(d)
    finally:
        d.write_evidence()
        d.close()
    failed = [check for check in d.checks if not check["ok"]]
    print("\n%d checks, %d failed" % (len(d.checks), len(failed)))
    for check in failed:
        print("  FAILED: %s %s" % (check["name"], check["detail"]))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
