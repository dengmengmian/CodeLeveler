#!/usr/bin/env python3
"""Real-PTY, real-model dogfood for instructions appended during a running turn.

This harness never injects a message through an internal API. It boots the real
`leveler tui` on a real pseudo-terminal against an isolated copy of the PermHub
fixture, types the follow-up into the real composer, and reads the frames back.
A recording HTTP proxy in front of the configured DeepSeek gateway captures the
exact request bodies that left the provider adapter, which is how DELIVERED is
proven rather than inferred.

Usage:
    python3 scripts/append_instruction_dogfood.py case1
    python3 scripts/append_instruction_dogfood.py case2 --repeat 3
    python3 scripts/append_instruction_dogfood.py all

Set LEVELER_APPEND_EVIDENCE_ROOT to choose a persistent archive directory and
LEVELER_BIN (or --binary) to select the binary under test. Each run receives a
new private directory, identity.json, and a final SHA256SUMS manifest.

Evidence is written under ~/CodeLeveler-append-instruction-evidence/<unique-run>/:
  frames/*.txt            decoded TUI screen at each checkpoint
  requests/*.json         full model request body, one file per request
  proxy.jsonl             request start/end timestamps and response summary
  database.json           sessions/turns/messages read back from SQLite
  sessions.sqlite3        transactionally consistent SQLite checkpoint, including WAL
  tests.txt              the fixture's own test run after the turn
  probe.json             independent HTTP checks against the resulting app
  workspace/             the resulting repository (without .git)
  workspace.diff         git diff of the resulting repository
  result.json             assertions and timeline
  steps.log              human-readable run log

The binary must be built from the source under test. A dirty working tree
makes the built identity refuse its own daemon, so the TUI is driven with
--in-process; that is still the real client, reducer, renderer, runtime and
provider adapter, not a test backend.
"""

from __future__ import annotations

import argparse
import hashlib
import html.parser
import importlib.util
import json
import os
import re
import shutil
import signal
import socket
import sqlite3
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "debug", "leveler")
FIXTURE = os.path.join(ROOT, "testdata", "append-instruction-dogfood", "permhub")


def evidence_root() -> str:
    """Keep the authoritative evidence outside Cargo's disposable build tree."""
    return os.path.abspath(os.path.expanduser(
        os.environ.get("LEVELER_APPEND_EVIDENCE_ROOT")
        or "~/CodeLeveler-append-instruction-evidence"
    ))


def create_evidence_dir(name: str) -> str:
    root = evidence_root()
    os.makedirs(root, exist_ok=True)
    return tempfile.mkdtemp(prefix="%s-%s-" % (time.strftime("%Y%m%d-%H%M%S"), name), dir=root)


def file_sha256(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_evidence_identity(directory: str, binary: str):
    """Record source and binary independently; a dirty SHA is not build proof."""
    def git(*args):
        return subprocess.check_output(["git", *args], cwd=ROOT)

    untracked = {}
    for relative in git("ls-files", "--others", "--exclude-standard", "-z").split(b"\0"):
        if relative:
            path = os.fsdecode(relative)
            full_path = os.path.join(ROOT, path)
            if os.path.isfile(full_path) and not os.path.islink(full_path):
                untracked[path] = file_sha256(full_path)
    identity = {
        "started_at_utc": datetime.utcnow().isoformat() + "Z",
        "source_head": git("rev-parse", "HEAD").decode().strip(),
        "source_diff_sha256": hashlib.sha256(git("diff", "--binary", "HEAD")).hexdigest(),
        "source_untracked_sha256": untracked,
        "binary_path": os.path.abspath(binary),
        "binary_sha256": file_sha256(binary),
        "build_binding": "Source snapshot and binary digest recorded independently; see build command log for provenance.",
    }
    with open(os.path.join(directory, "identity.json"), "x", encoding="utf-8") as fh:
        json.dump(identity, fh, ensure_ascii=False, indent=2)


def finalize_evidence(directory: str):
    """Seal only after writers close, so checksums describe final artifacts."""
    identity_path = os.path.join(directory, "identity.json")
    with open(identity_path, encoding="utf-8") as fh:
        identity = json.load(fh)
    configs = []
    requests = os.path.join(directory, "requests")
    if os.path.isdir(requests):
        for name in sorted(os.listdir(requests)):
            if not name.endswith(".json"):
                continue
            with open(os.path.join(requests, name), encoding="utf-8") as fh:
                body = json.load(fh)
            config = {key: body[key] for key in ("model", "reasoning_effort", "max_tokens", "max_completion_tokens", "temperature") if key in body}
            if config not in configs:
                configs.append(config)
    identity["observed_model_configs"] = configs
    identity["finished_at_utc"] = datetime.utcnow().isoformat() + "Z"
    with open(identity_path, "w", encoding="utf-8") as fh:
        json.dump(identity, fh, ensure_ascii=False, indent=2)
    with open(os.path.join(directory, "SHA256SUMS"), "x", encoding="utf-8") as manifest:
        for base, dirs, files in os.walk(directory):
            dirs.sort()
            for name in sorted(files):
                path = os.path.join(base, name)
                if name == "SHA256SUMS" and base == directory:
                    continue
                if not os.path.islink(path):
                    manifest.write("%s  %s\n" % (file_sha256(path), os.path.relpath(path, directory)))


def load_pty_module():
    path = os.path.join(ROOT, "scripts", "pty_dogfood.py")
    spec = importlib.util.spec_from_file_location("pty_dogfood", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# ---------------------------------------------------------------------------
# Recording proxy
# ---------------------------------------------------------------------------


class RecordingProxy:
    """Forwards OpenAI-wire requests to the real gateway and records them.

    The Authorization header is forwarded but never read or written. Request
    bodies contain the conversation, not credentials.
    """

    def __init__(self, evidence_dir: str, upstream_base: str):
        parsed = urllib.parse.urlsplit(upstream_base)
        if parsed.scheme != "https":
            raise SystemExit("the dogfood proxy expects an https upstream, got %r" % upstream_base)
        self.host = parsed.hostname
        self.port = parsed.port or 443
        self.prefix = parsed.path.rstrip("/")
        self.path = os.path.join(evidence_dir, "proxy.jsonl")
        self.request_dir = os.path.join(evidence_dir, "requests")
        os.makedirs(self.request_dir, exist_ok=True)
        self.lock = threading.Lock()
        self.seq = 0
        self.tag = {"case": "boot", "label": ""}
        self._fh = open(self.path, "w", encoding="utf-8")
        proxy = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                proxy.handle(self, body)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        # ThreadingMixIn.server_close joins non-daemon workers. Keep the
        # evidence writer open until every handler records request_end.
        self.server.daemon_threads = False
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def base_url(self) -> str:
        return "http://127.0.0.1:%d%s" % (self.server.server_address[1], self.prefix)

    def handle(self, handler: BaseHTTPRequestHandler, body: bytes):
        try:
            request = json.loads(body.decode("utf-8", "replace"))
        except Exception:
            request = {}
        with self.lock:
            self.seq += 1
            seq = self.seq
            tag = dict(self.tag)
        model = request.get("model")
        messages = request.get("messages") or []
        user_texts = []
        for message in messages:
            content = message.get("content")
            if message.get("role") == "user" and isinstance(content, str):
                user_texts.append(content)
        tool_names = [
            (tool.get("function") or {}).get("name") or tool.get("name")
            for tool in (request.get("tools") or [])
        ]
        body_path = os.path.join(self.request_dir, "%04d.json" % seq)
        with open(body_path, "w", encoding="utf-8") as fh:
            json.dump(request, fh, ensure_ascii=False, indent=1)

        started = time.time()
        self._write(
            {
                "kind": "request_start",
                "seq": seq,
                "t": started,
                "tag": tag,
                "model": model,
                "message_count": len(messages),
                "user_texts": user_texts,
                "tool_names": tool_names,
                "body": os.path.relpath(body_path, os.path.dirname(self.path)),
            }
        )

        headers = {
            key: value
            for key, value in handler.headers.items()
            if key.lower() not in ("host", "connection", "content-length")
        }
        conn = None
        status = None
        error = None
        finish_reasons = []
        saw_tool_call = False
        received = 0
        try:
            conn = ssl.create_default_context().wrap_socket(
                socket.create_connection((self.host, self.port), timeout=60),
                server_hostname=self.host,
            )
            conn.settimeout(600)
            lines = ["POST %s HTTP/1.1" % handler.path, "Host: %s" % self.host]
            for key, value in headers.items():
                lines.append("%s: %s" % (key, value))
            lines.append("Connection: close")
            lines.append("Content-Length: %d" % len(body))
            conn.sendall(("\r\n".join(lines) + "\r\n\r\n").encode() + body)

            buffer = b""
            while b"\r\n\r\n" not in buffer:
                chunk = conn.recv(1 << 16)
                if not chunk:
                    break
                buffer += chunk
            head, _, rest = buffer.partition(b"\r\n\r\n")
            head_lines = head.decode("iso-8859-1", "replace").split("\r\n")
            if head_lines:
                match = re.match(r"HTTP/\d\.\d\s+(\d+)", head_lines[0])
                if match:
                    status = int(match.group(1))
            handler.send_response(status or 502)
            handler.send_header("Content-Type", "text/event-stream")
            handler.send_header("Connection", "close")
            handler.end_headers()
            chunks = [rest] if rest else []
            while True:
                chunk = conn.recv(1 << 16)
                if not chunk:
                    break
                chunks.append(chunk)
            for chunk in chunks:
                received += len(chunk)
                text = chunk.decode("utf-8", "replace")
                finish_reasons.extend(re.findall(r'"finish_reason"\s*:\s*"([^"]+)"', text))
                if '"tool_calls"' in text:
                    saw_tool_call = True
                try:
                    handler.wfile.write(chunk)
                    handler.wfile.flush()
                except Exception:
                    break
        except Exception as exc:  # recorded, never hidden
            error = "%s: %s" % (type(exc).__name__, exc)
            if status is None:
                try:
                    handler.send_response(502)
                    handler.send_header("Content-Type", "text/plain")
                    handler.send_header("Connection", "close")
                    handler.end_headers()
                except Exception:
                    pass
        finally:
            if conn is not None:
                try:
                    conn.close()
                except Exception:
                    pass
            handler.close_connection = True
            ended = time.time()
            self._write(
                {
                    "kind": "request_end",
                    "seq": seq,
                    "t": ended,
                    "duration": ended - started,
                    "status": status,
                    "bytes": received,
                    "finish_reasons": finish_reasons,
                    "saw_tool_call": saw_tool_call,
                    "error": error,
                }
            )

    def _write(self, record: dict):
        with self.lock:
            self._fh.write(json.dumps(record, ensure_ascii=False) + "\n")
            self._fh.flush()

    def records(self) -> list[dict]:
        with self.lock:
            self._fh.flush()
        out = []
        for line in open(self.path, encoding="utf-8"):
            try:
                out.append(json.loads(line))
            except Exception:
                continue
        return out

    def requests(self, started_after: float | None = None) -> list[dict]:
        return [
            record
            for record in self.records()
            if record.get("kind") == "request_start"
            and (started_after is None or record.get("t", 0) >= started_after)
        ]

    def ends(self, after: float = 0) -> list[dict]:
        return [
            record
            for record in self.records()
            if record.get("kind") == "request_end" and record.get("t", 0) >= after
        ]

    def in_flight(self, require_tools: bool = False) -> dict | None:
        started = {}
        for record in self.records():
            if record.get("kind") == "request_start":
                started[record["seq"]] = record
            elif record.get("kind") == "request_end":
                started.pop(record.get("seq"), None)
        candidates = [
            record
            for record in started.values()
            if not require_tools or record.get("tool_names")
        ]
        return next(iter(candidates), None)

    def main_request_seqs(self) -> set[int]:
        return {
            record["seq"]
            for record in self.records()
            if record.get("kind") == "request_start" and record.get("tool_names")
        }

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self._fh.close()


# ---------------------------------------------------------------------------
# Workspace and process environment
# ---------------------------------------------------------------------------


def source_config() -> str:
    path = os.path.expanduser("~/.leveler/config.toml")
    if not os.path.exists(path):
        raise SystemExit("no ~/.leveler/config.toml to copy")
    return path


def upstream_base() -> str:
    text = open(source_config(), encoding="utf-8").read()
    match = re.search(
        r"\[providers\.deepseek\][^\[]*?base_url\s*=\s*\"([^\"]+)\"", text, re.S
    )
    if not match:
        raise SystemExit("no [providers.deepseek] base_url in ~/.leveler/config.toml")
    return match.group(1)


def rewrite_base_url(text: str, proxy_url: str) -> str:
    out = []
    section = None
    replaced = 0
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped
        if section == "[providers.deepseek]" and re.match(r"\s*base_url\s*=", line):
            line = 'base_url = "%s"' % proxy_url
            replaced += 1
        out.append(line)
    if replaced != 1:
        raise SystemExit(
            "expected exactly one base_url under [providers.deepseek], replaced %d" % replaced
        )
    return "\n".join(out) + "\n"


def build_workspace(proxy_url: str):
    root = tempfile.mkdtemp(prefix="lv-append-", dir="/tmp")
    home = os.path.join(root, "home")
    repo = os.path.join(root, "repo")
    os.makedirs(home)
    shutil.copytree(
        FIXTURE,
        repo,
        ignore=shutil.ignore_patterns(".slow_started", "__pycache__", ".git"),
    )
    os.makedirs(os.path.join(repo, ".leveler"), exist_ok=True)
    with open(os.path.join(repo, ".leveler", "config.yaml"), "w", encoding="utf-8") as fh:
        fh.write("{}\n")
    open(os.path.join(repo, ".gitignore"), "a", encoding="utf-8").write(
        ".slow_started\n__pycache__/\n"
    )
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
    subprocess.run(
        [
            "git",
            "-c",
            "user.email=permhub@example.com",
            "-c",
            "user.name=permhub",
            "commit",
            "-q",
            "-m",
            "seed permhub",
        ],
        cwd=repo,
        check=True,
    )
    config_path = os.path.join(home, "config.toml")
    with open(source_config(), encoding="utf-8") as fh:
        config = fh.read()
    with open(config_path, "w", encoding="utf-8") as fh:
        fh.write(rewrite_base_url(config, proxy_url))
    os.chmod(config_path, 0o600)
    env = dict(os.environ)
    env.update(
        {
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "LEVELER_HOME": home,
            "LEVELER_LOG": "info",
        }
    )
    return root, home, repo, env


# ---------------------------------------------------------------------------
# SQLite evidence helpers
# ---------------------------------------------------------------------------


def session_database(home: str) -> str | None:
    found = []
    for base, _dirs, files in os.walk(os.path.join(home, "state")):
        if "sessions.db" in files:
            found.append(os.path.join(base, "sessions.db"))
    if not found:
        return None
    return max(found, key=os.path.getmtime)


def snapshot_database(home: str, directory: str) -> str | None:
    """Preserve the pre-cleanup checkpoint through SQLite, including its WAL.

    Copy only the session store, never the home directory or provider config.
    This snapshot records the scenario's state before quitting clients can
    cancel pending work; it does not claim to be a post-shutdown observation.
    """
    source_path = session_database(home)
    if source_path is None:
        return None
    target = os.path.join(directory, "sessions.sqlite3")
    with open(target, "xb"):
        pass
    source = sqlite3.connect("file:%s?mode=ro" % urllib.parse.quote(source_path), uri=True)
    destination = sqlite3.connect(target)
    try:
        source.backup(destination)
        # The archive must be one self-contained file when reopened, even if
        # the live source uses WAL. This changes only the destination store.
        mode = destination.execute("PRAGMA journal_mode=DELETE").fetchone()[0]
        if mode != "delete":
            raise RuntimeError("archived session database is not self-contained")
        if destination.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
            raise RuntimeError("archived session database failed integrity_check")
    finally:
        destination.close()
        source.close()
    # A backup from a WAL store may leave a shared-memory sidecar even after
    # SQLite switches the closed destination to DELETE mode.
    for suffix in ("-wal", "-shm"):
        sidecar = target + suffix
        if os.path.exists(sidecar):
            os.remove(sidecar)
    return target


def query(home: str, sql: str, params=()) -> list[dict]:
    path = session_database(home)
    if not path:
        return []
    try:
        conn = sqlite3.connect(path)
        conn.row_factory = sqlite3.Row
        rows = [dict(row) for row in conn.execute(sql, params)]
        conn.close()
        return rows
    except Exception:
        return []


def turns(home: str) -> list[dict]:
    return query(
        home,
        "SELECT id, session_id, ordinal, status, created_at, finished_at "
        "FROM turns ORDER BY ordinal, created_at",
    )


def messages(home: str, text: str | None = None) -> list[dict]:
    rows = query(
        home,
        "SELECT id, ordinal, turn_id, created_at, payload, input_id "
        "FROM session_messages ORDER BY ordinal",
    )
    if text is None:
        return rows
    return [row for row in rows if text in (row.get("payload") or "")]


def payload_role(row: dict) -> str | None:
    try:
        return json.loads(row.get("payload") or "{}").get("role")
    except Exception:
        return None


def user_messages(home: str, text: str | None = None) -> list[dict]:
    """Rows the user authored. Assistant echoes of the same text are not a
    persistence witness for the user's instruction."""
    rows = messages(home, text)
    return [row for row in rows if payload_role(row) == "user"]


def latest_session(home: str) -> str | None:
    rows = query(home, "SELECT id FROM sessions ORDER BY created_at DESC LIMIT 1")
    return rows[0]["id"] if rows else None


def journal_rows(home: str, text: str | None = None) -> list[dict]:
    """The durable input journal: accepted, not yet/ever consumed, or cancelled."""
    table = query(
        home,
        "SELECT name FROM sqlite_master WHERE type='table' AND name='session_inputs'",
    )
    if not table:
        return []
    rows = query(
        home,
        "SELECT id, session_id, turn_id, content, state, reason, ordinal, "
        "created_at, settled_at FROM session_inputs ORDER BY created_at",
    )
    if text is None:
        return rows
    return [row for row in rows if text in (row.get("content") or "")]


def iso_epoch(value: str | None) -> float | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value).timestamp()
    except Exception:
        return None


def sanitize(value):
    if isinstance(value, bytes):
        return "<%d bytes sha256=%s>" % (len(value), hashlib.sha256(value).hexdigest()[:12])
    if isinstance(value, str) and len(value) > 2000:
        return value[:2000] + "…<%d chars>" % len(value)
    return value


def database_evidence(home: str, injected: list[str]) -> dict:
    data = {"path": session_database(home), "turns": turns(home)}
    msgs = []
    for row in messages(home):
        item = {key: sanitize(value) for key, value in row.items()}
        payload = row.get("payload") or ""
        item["role"] = None
        item["text"] = None
        try:
            parsed = json.loads(payload)
            item["role"] = parsed.get("role")
            content = parsed.get("content") or []
            texts = [part.get("text") for part in content if isinstance(part, dict) and part.get("text")]
            item["text"] = "\n".join(texts)[:1200]
        except Exception:
            pass
        item["contains_injected"] = [
            text for text in injected if text in payload
        ]
        msgs.append(item)
    data["messages"] = msgs
    data["injected_rows"] = [
        msgs_row for msgs_row in msgs if msgs_row["contains_injected"]
    ]
    data["model_requests"] = query(
        home,
        "SELECT id, turn_id, model, input_tokens, output_tokens, finish_reason, "
        "latency_ms, created_at FROM model_requests ORDER BY created_at",
    )
    data["session_inputs"] = query(
        home,
        "SELECT id, session_id, turn_id, content, state, reason, ordinal, "
        "created_at, settled_at FROM session_inputs ORDER BY created_at",
    )
    data["events"] = query(
        home,
        "SELECT id, session_id, turn_id, sequence, type, payload, created_at "
        "FROM events ORDER BY sequence",
    )
    data["sessions"] = query(home, "SELECT id, status, model, created_at, updated_at FROM sessions")
    return data


# ---------------------------------------------------------------------------
# Independent project probe
# ---------------------------------------------------------------------------


PROBE_SCRIPT = r'''
import json, re, sys, threading, urllib.error, urllib.parse, urllib.request
from html.parser import HTMLParser

from app import make_server

server = make_server(port=0)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = "http://127.0.0.1:%d" % server.server_address[1]

def get(path):
    try:
        with urllib.request.urlopen(base + path, timeout=5) as response:
            return {"status": response.status, "body": response.read().decode("utf-8", "replace")}
    except urllib.error.HTTPError as error:
        return {"status": error.code, "body": error.read().decode("utf-8", "replace")}
    except Exception as error:
        return {"status": None, "body": "", "error": "%s: %s" % (type(error).__name__, error)}

def post(path, data):
    body = urllib.parse.urlencode(data).encode()
    request = urllib.request.Request(base + path, data=body, method="POST")
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return {"status": response.status, "body": response.read().decode("utf-8", "replace")}
    except urllib.error.HTTPError as error:
        return {"status": error.code, "body": error.read().decode("utf-8", "replace")}
    except Exception as error:
        return {"status": None, "body": "", "error": "%s: %s" % (type(error).__name__, error)}

class Forms(HTMLParser):
    def __init__(self):
        super().__init__()
        self.forms = []
        self.current = None
    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "form":
            self.current = {"action": attrs.get("action", ""), "method": attrs.get("method", "get").lower(), "inputs": []}
        elif tag in ("input", "button", "select") and self.current is not None:
            self.current["inputs"].append({"tag": tag, "name": attrs.get("name"), "value": attrs.get("value", ""), "type": attrs.get("type", "")})
    def handle_endtag(self, tag):
        if tag == "form" and self.current is not None:
            self.forms.append(self.current)
            self.current = None

out = {}
accounts = get("/accounts")
out["accounts_status"] = accounts["status"]
out["accounts_has_all"] = all(name in accounts["body"] for name in ("Alice Chen", "Bob Li", "Carol Wang"))
out["search_by_name"] = "Alice Chen" in get("/accounts?q=Alice")["body"] and "Bob Li" not in get("/accounts?q=Alice")["body"]
out["search_by_email"] = "Bob Li" in get("/accounts?q=bob%40example.com")["body"]
status_match = re.search(r'name=["\']([^"\']*status[^"\']*)["\']', accounts["body"])
status_candidates = []
if status_match:
    status_candidates.append(status_match.group(1))
status_candidates.extend(["status", "state"])
status_candidates = list(dict.fromkeys(status_candidates))
option_values = re.findall(r'<option[^>]*value=["\']([^"\']+)["\']', accounts["body"])
active_value = next((value for value in option_values if "active" in value.lower() and "inactive" not in value.lower() and "disabled" not in value.lower()), "active")
disabled_value = next((value for value in option_values if "disabled" in value.lower() or "inactive" in value.lower()), "disabled")
out["status_param"] = status_candidates[0]
out["status_values"] = [active_value, disabled_value]
out["status_filter"] = False
out["status_filter_disabled"] = False
for status_param in status_candidates:
    active = get("/accounts?" + urllib.parse.urlencode({status_param: active_value}))
    disabled = get("/accounts?" + urllib.parse.urlencode({status_param: disabled_value}))
    active_ok = active["status"] == 200 and "Alice Chen" in active["body"] and "Carol Wang" in active["body"] and "Bob Li" not in active["body"]
    disabled_ok = disabled["status"] == 200 and "Bob Li" in disabled["body"] and "Alice Chen" not in disabled["body"]
    if active_ok and disabled_ok:
        out["status_param"] = status_param
        out["status_filter"] = True
        out["status_filter_disabled"] = True
        break
roles = get("/roles")
out["roles_status"] = roles["status"]
out["roles_permissions"] = "role.write" in roles["body"] and "account.read" in roles["body"]

admin_path = "/admin"
admin = get(admin_path)
# The user asked for admin management, not for a particular path. Accept the
# first route that really serves it so a working /admins page is not failed
# because this probe guessed /admin.
if admin["status"] != 200 or "Alice Chen" not in admin["body"]:
    for candidate in ("/admins", "/admin/accounts", "/administrators"):
        probe = get(candidate)
        if probe["status"] == 200 and "Alice Chen" in probe["body"]:
            admin_path = candidate
            admin = probe
            break
out["admin_path"] = admin_path
out["admin_status"] = admin["status"]
out["admin_has_alice"] = "Alice Chen" in admin["body"]
out["admin_has_permissions"] = "role.write" in admin["body"]
admin_query_match = re.search(
    r'<input[^>]*name=["\'](q|query|search|name)["\']', admin["body"], re.I
) or re.search(r'<input[^>]*name=["\']([^"\']+)["\']', admin["body"])
admin_query = admin_query_match.group(1) if admin_query_match else "q"
out["admin_query_param"] = admin_query
admin_search = get(admin_path + "?" + urllib.parse.urlencode({admin_query: "Alice"}))
out["admin_search_supported"] = admin_search["status"] == 200 and "Alice Chen" in admin_search["body"]
out["admin_search_excludes"] = "Bob Li" not in admin_search["body"]
out["admin_enable_disable_text"] = any(word in admin["body"] for word in ("启用", "禁用", "enable", "disable"))
forms = Forms()
forms.feed(admin["body"])
out["admin_forms"] = forms.forms
form_result = None
matching = [
    form
    for form in forms.forms
    if any(
        word in (form["action"] or admin_path).lower()
        for word in ("admin", "toggle", "status")
    )
]
# Prefer the action that changes state over the GET search form on the same
# page; otherwise the probe would re-fetch /admin and never exercise toggle.
matching.sort(key=lambda form: form["method"] != "post")
for form in matching:
    action = form["action"] or admin_path
    payload = {item["name"]: item["value"] for item in form["inputs"] if item.get("name")}
    if form["method"] == "post":
        form_result = post(action, payload)
    else:
        form_result = get(action + "?" + urllib.parse.urlencode(payload))
    break
out["admin_form_result"] = form_result
if form_result is not None:
    after = get(admin_path)
    out["admin_after_form"] = after["body"]
    out["admin_status_changed"] = after["body"] != admin["body"]

health = get("/health")
out["health_status"] = health["status"]
out["health_body"] = health["body"][:300]

print(json.dumps(out, ensure_ascii=False))
server.shutdown()
server.server_close()
'''


def probe_project(repo: str, evidence: str) -> dict:
    probe_path = os.path.join(evidence, "probe.json")
    try:
        result = subprocess.run(
            [sys.executable, "-c", PROBE_SCRIPT],
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=45,
        )
        text = result.stdout.strip().splitlines()[-1] if result.stdout.strip() else ""
        data = json.loads(text) if text.startswith("{") else {
            "error": "probe produced no JSON",
            "stdout": result.stdout[-2000:],
            "stderr": result.stderr[-2000:],
        }
    except Exception as exc:
        data = {"error": "%s: %s" % (type(exc).__name__, exc)}
    with open(probe_path, "w", encoding="utf-8") as fh:
        json.dump(data, fh, ensure_ascii=False, indent=2)
    return data


def run_project_tests(repo: str, evidence: str) -> dict:
    marker = os.path.join(repo, ".slow_started")
    if os.path.exists(marker):
        os.unlink(marker)
    started = time.time()
    try:
        result = subprocess.run(
            [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-v"],
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=240,
        )
        output = (result.stdout or "") + (result.stderr or "")
        ok = result.returncode == 0
        summary = {"ok": ok, "returncode": result.returncode, "duration": time.time() - started}
    except Exception as exc:
        output = "%s: %s" % (type(exc).__name__, exc)
        summary = {"ok": False, "error": output, "duration": time.time() - started}
    with open(os.path.join(evidence, "tests.txt"), "w", encoding="utf-8") as fh:
        fh.write(output)
    summary["tail"] = output[-1500:]
    summary["ran"] = re.findall(r"Ran (\d+) tests", output)
    return summary


def snapshot_workspace(repo: str, evidence: str):
    target = os.path.join(evidence, "workspace")
    if os.path.exists(target):
        shutil.rmtree(target)
    shutil.copytree(
        repo,
        target,
        ignore=shutil.ignore_patterns(".git", "__pycache__", ".slow_started"),
    )
    diff = subprocess.run(
        ["git", "diff", "--no-color"], cwd=repo, capture_output=True, text=True
    ).stdout
    status = subprocess.run(
        ["git", "status", "--porcelain"], cwd=repo, capture_output=True, text=True
    ).stdout
    with open(os.path.join(evidence, "workspace.diff"), "w", encoding="utf-8") as fh:
        fh.write(diff)
    with open(os.path.join(evidence, "workspace-status.txt"), "w", encoding="utf-8") as fh:
        fh.write(status)


# ---------------------------------------------------------------------------
# TUI session
# ---------------------------------------------------------------------------


class Session:
    def __init__(self, env: dict, repo: str, bin_path: str, evidence: str, log):
        self.pty_module = load_pty_module()
        self.env = env
        self.repo = repo
        self.bin = bin_path
        self.evidence = evidence
        self.frames = os.path.join(evidence, "frames")
        os.makedirs(self.frames, exist_ok=True)
        self.pty = None
        self.log = log
        self.timeline = []
        self.proxy: RecordingProxy | None = None

    def note(self, text: str):
        line = "[%s] %s" % (time.strftime("%H:%M:%S"), text)
        print(line)
        self.log.write(line + "\n")
        self.log.flush()
        self.timeline.append({"t": time.time(), "text": text})

    def dump(self, label: str) -> str:
        path = os.path.join(self.frames, label + ".txt")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(self.pty.screen.text())
            fh.write("\n")
        raw = os.path.join(self.frames, label + ".raw")
        with open(raw, "wb") as fh:
            fh.write(bytes(self.pty.raw))
        return path

    def spawn(self, session: str | None = None):
        argv = [
            self.bin,
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
        self.pty = self.pty_module.Pty(argv, self.env, 120, 48)
        self.pty.wait_for("CodeLeveler", timeout=60)
        self.pty.wait_stable(1.5, 60)
        self.dump("boot")

    def text(self) -> str:
        return self.pty.screen.text()

    def wait_for(self, needle: str, timeout: float) -> bool:
        return self.pty.wait_for(needle, timeout)

    def type_and_enter(self, text: str, settle: float = 0.4):
        self.note("type: " + text)
        self.pty.write(text)
        time.sleep(settle)
        self.pty.key("\r")

    def send_queued(self):
        """Tab into 待发送 and press Enter: the explicit send/steer action."""
        self.note("action: Tab -> 待发送, Enter -> send")
        self.pty.key("\t")
        time.sleep(0.2)
        self.pty.key("\r")

    def quit(self):
        if self.pty and self.pty.alive():
            self.pty.key("\x03")
            time.sleep(0.6)
            self.pty.key("\x03")
            deadline = time.time() + 30
            while time.time() < deadline and self.pty.alive():
                time.sleep(0.2)
            self.pty.close()
            self.note("quit")

    def kill(self):
        self.note("SIGKILL pid %d" % self.pty.proc.pid)
        try:
            os.killpg(os.getpgid(self.pty.proc.pid), signal.SIGKILL)
        except Exception as exc:
            self.note("kill failed: " + str(exc))
        time.sleep(1.0)
        self.pty.close()


# ---------------------------------------------------------------------------
# Result and case base
# ---------------------------------------------------------------------------


class Result:
    def __init__(self, case: str):
        self.case = case
        self.checks = []
        self.notes = []
        self.timeline = []

    def check(self, name: str, ok, detail: str = ""):
        self.checks.append({"name": name, "ok": ok, "detail": detail})
        mark = "PASS" if ok is True else ("FAIL" if ok is False else "UNKNOWN")
        print("  %-7s %s%s" % (mark, name, (" — " + detail) if detail else ""))
        return ok

    def note(self, text: str):
        self.notes.append(text)
        print("  note    " + text)

    def write(self, path: str):
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(
                {
                    "case": self.case,
                    "checks": self.checks,
                    "notes": self.notes,
                    "timeline": self.timeline,
                },
                fh,
                ensure_ascii=False,
                indent=2,
            )


BASE_PROMPT = (
    "请先运行项目的全部测试：python3 -m unittest discover -s tests -v"
    "（调用命令时设置 timeout_seconds=120，让它在本次调用内完成）。\n"
    "然后完善权限管理页面：账户列表、角色与权限展示、账户搜索都要可用。"
    "修改后再次运行全部测试，并实际请求页面确认"
    "（用 tests 里的 make_server 或临时端口的一次性脚本；不要启动长期运行的服务，也不要 wait_task 一个服务器）。"
    "不要只回复说明，要实际修改和验证。"
)


class CaseRun:
    def __init__(self, name: str, args, repeat: int = 0):
        suffix = "-r%d" % repeat if repeat else ""
        self.name = name
        self.evidence = create_evidence_dir(name + suffix)
        write_evidence_identity(self.evidence, args.binary)
        self.result = Result(name)
        self.log = open(os.path.join(self.evidence, "steps.log"), "w", encoding="utf-8")
        self.proxy = RecordingProxy(self.evidence, upstream_base())
        self.root, self.home, self.repo, self.env = build_workspace(self.proxy.base_url())
        self.session = Session(self.env, self.repo, args.binary, self.evidence, self.log)
        self.session.proxy = self.proxy
        self.result.note("workspace: " + self.repo)
        self.result.note("evidence: " + self.evidence)
        self.result.note("proxy: " + self.proxy.base_url())
        self.session_id = None
        self.injected = []
        self.injections = []

    def finish(self):
        try:
            request_rows = query(
                self.home, "SELECT input_tokens, output_tokens FROM model_requests"
            )
            self.result.note(
                "tokens: requests=%d input=%d output=%d"
                % (
                    len(request_rows),
                    sum(row.get("input_tokens") or 0 for row in request_rows),
                    sum(row.get("output_tokens") or 0 for row in request_rows),
                )
            )
            snapshot = snapshot_database(self.home, self.evidence)
            self.result.note("pre-cleanup SQLite checkpoint: " + (snapshot or "unavailable: no session database created"))
            snapshot_workspace(self.repo, self.evidence)
            with open(os.path.join(self.evidence, "database.json"), "w", encoding="utf-8") as fh:
                json.dump(
                    database_evidence(self.home, self.injected),
                    fh,
                    ensure_ascii=False,
                    indent=2,
                )
            self.result.timeline = self.session.timeline
            self.result.write(os.path.join(self.evidence, "result.json"))
        finally:
            try:
                self.session.quit()
            finally:
                self.proxy.close()
                self.log.close()
                finalize_evidence(self.evidence)

    # -- timing helpers ---------------------------------------------------

    def wait_marker(self, timeout: float = 300) -> bool:
        marker = os.path.join(self.repo, ".slow_started")
        deadline = time.time() + timeout
        while time.time() < deadline:
            if os.path.exists(marker):
                return True
            if not self.session.pty.alive():
                return False
            time.sleep(0.1)
        return False

    def wait_in_flight(
        self, min_age: float = 0.8, timeout: float = 300, require_tools: bool = True
    ) -> dict | None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            record = self.proxy.in_flight(require_tools=require_tools)
            if record and time.time() - record["t"] >= min_age:
                return record
            if not self.session.pty.alive():
                return None
            time.sleep(0.05)
        return None

    def wait_late_in_flight(
        self, runs: int = 2, min_age: float = 1.0, timeout: float = 600
    ) -> dict | None:
        """An in-flight main request whose context already holds `runs` test
        results. The base prompt runs the suite before and after the edits, so
        the request after the second result is the model's final window."""
        deadline = time.time() + timeout
        counts: dict[int, int] = {}
        while time.time() < deadline:
            ended = {
                record["seq"]
                for record in self.proxy.ends()
                if record.get("kind") == "request_end"
            }
            main = self.proxy.main_request_seqs()
            for record in self.proxy.requests():
                if record["seq"] in ended or record["seq"] not in main:
                    continue
                if time.time() - record["t"] < min_age:
                    continue
                count = counts.get(record["seq"])
                if count is None:
                    path = os.path.join(self.evidence, record.get("body") or "")
                    try:
                        with open(path, encoding="utf-8", errors="replace") as fh:
                            count = fh.read().count("Ran ")
                    except OSError:
                        count = 0
                    counts[record["seq"]] = count
                if count >= runs:
                    return record
            rows = turns(self.home)
            if rows and all(row.get("status") != "running" for row in rows):
                return None
            if not self.session.pty.alive():
                return None
            time.sleep(0.25)
        return None

    def wait_final_response_end(self, after: float, timeout: float = 600) -> dict | None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            main = self.proxy.main_request_seqs()
            for record in self.proxy.ends(after):
                if record.get("seq") not in main:
                    continue
                if "stop" in (record.get("finish_reasons") or []) and not record.get(
                    "saw_tool_call"
                ):
                    return record
            if not self.session.pty.alive():
                return None
            time.sleep(0.05)
        return None

    def wait_turns(self, expected: int, timeout: float = 600) -> list[dict] | None:
        deadline = time.time() + timeout
        while time.time() < deadline:
            rows = turns(self.home)
            if len(rows) >= expected and all(row.get("status") != "running" for row in rows):
                return rows
            if not self.session.pty.alive():
                return rows or None
            time.sleep(0.25)
        return turns(self.home)

    def confirm_recovery(self, timeout: float = 60) -> bool:
        """Acknowledge the crash-recovery overlay Resume puts up.

        The runtime pauses submission while an interrupted tool call has an
        unknown outcome; the overlay cannot be dismissed by typing. The cursor
        starts on cancel, so Down moves to 确认恢复 and Enter releases it.
        """
        deadline = time.time() + timeout
        started = time.time()
        asked = False
        while time.time() < deadline:
            screen = self.session.text()
            if "恢复确认" in screen:
                # Ctrl+N is the selection model's move_down; a lone Ctrl+N
                # avoids the PTY splitting an ESC [ B sequence into Esc + text.
                self.session.pty.write("\x0e")
                time.sleep(0.3)
                self.session.pty.write("\r")
                time.sleep(1.0)
                if "恢复确认" in self.session.text():
                    self.session.pty.write("\x1b[B")
                    time.sleep(0.3)
                    self.session.pty.write("\r")
                    time.sleep(1.0)
                self.session.dump("2b-recovery-confirmed")
                return "恢复确认" not in self.session.text()
            if "任务恢复已暂停" in screen and not asked:
                # The runtime named the block but the overlay is not on screen
                # yet; `/recover` opens the same confirmation explicitly.
                self.session.pty.write("/recover\r")
                asked = True
                time.sleep(1.0)
                continue
            if "任务恢复已暂停" not in screen and time.time() - started >= 5 and not asked:
                self.session.pty.write("/recover\r")
                asked = True
                time.sleep(1.0)
                continue
            if asked and "任务恢复已暂停" not in screen and "恢复确认" not in screen:
                # `/recover` reported that there is nothing to acknowledge.
                return True
            time.sleep(0.2)
        return False

    def wait_all_settled(self, timeout: float = 600) -> list[dict] | None:
        """Wait until every turn row is terminal and the row count is stable."""
        deadline = time.time() + timeout
        stable_since = None
        last_count = None
        while time.time() < deadline:
            rows = turns(self.home)
            running = [row for row in rows if row.get("status") == "running"]
            if rows and not running:
                if last_count != len(rows):
                    last_count = len(rows)
                    stable_since = time.time()
                elif time.time() - stable_since >= 1.0:
                    return rows
            if not self.session.pty.alive():
                return rows or None
            time.sleep(0.25)
        return turns(self.home)

    def wait_request_with(self, text: str, after: float = 0, timeout: float = 600):
        deadline = time.time() + timeout
        while time.time() < deadline:
            for record in self.proxy.requests(started_after=after):
                if any(text in item for item in record.get("user_texts", [])):
                    return record
            time.sleep(0.1)
        return None

    # -- injection --------------------------------------------------------

    def inject_default(self, text: str, label: str):
        """The ordinary type-then-Enter path while a turn runs.

        Under the fixed product contract this is an append to the running
        task, not a held draft: the runtime must journal it before the ACK and
        deliver it at the next model round.
        """
        before = len(self.proxy.requests())
        before_turns = turns(self.home)
        at = time.time()
        self.session.type_and_enter(text)
        time.sleep(0.6)
        frame = self.session.dump(label)
        screen = self.session.text()
        state_line = any(
            marker in screen
            for marker in ("发送中", "已保存 · 等待投递", "已进入当前任务")
        )
        held = "待发送" in screen
        self.injected.append(text)
        record = {
            "text": text,
            "lane": "default-enter-steer",
            "at": at,
            "before_requests": before,
            "turn_running_at_inject": any(
                row.get("status") == "running" for row in before_turns
            ),
            "held": held,
            "delivery_state": state_line,
            "frame": frame,
        }
        self.injections.append(record)
        self.result.check(
            "RECEIVED — the composer accepted %r" % text,
            text in screen or held,
            "text visible in the TUI",
        )
        self.result.check(
            "VISIBLE — the append is painted with a delivery state",
            state_line,
            "one of 发送中 / 已保存 · 等待投递 / 已进入当前任务",
        )
        return record

    def inject_steer(self, text: str, label: str, settle: float = 0.6):
        """An explicit append while the turn runs: type and Enter.

        There is no separate key sequence any more; the default Enter is the
        append. The name is kept so the case matrix still distinguishes an
        intentional append from a stray one.
        """
        before = len(self.proxy.requests())
        before_turns = turns(self.home)
        at = time.time()
        self.session.note("steer: " + text)
        self.session.pty.write(text + "\r")
        time.sleep(settle)
        frame = self.session.dump(label)
        self.injected.append(text)
        record = {
            "text": text,
            "lane": "explicit-steer",
            "at": at,
            "before_requests": before,
            "turn_running_at_inject": any(
                row.get("status") == "running" for row in before_turns
            ),
            "held": "待发送" in self.session.text(),
            "frame": frame,
        }
        self.injections.append(record)
        self.result.check(
            "RECEIVED — the composer accepted %r" % text,
            text in self.session.text() or "待发送" in self.session.text(),
            "text visible in the TUI",
        )
        return record

    def inject_tab_lane(self, text: str, label: str):
        """Tab: the explicit 'process after this task' lane."""
        before = len(self.proxy.requests())
        before_turns = turns(self.home)
        at = time.time()
        self.session.note("tab-lane: " + text)
        self.session.pty.write(text)
        time.sleep(0.3)
        self.session.pty.key("\t")
        time.sleep(0.6)
        frame = self.session.dump(label)
        screen = self.session.text()
        self.injected.append(text)
        record = {
            "text": text,
            "lane": "tab-after-turn",
            "at": at,
            "before_requests": before,
            "turn_running_at_inject": any(
                row.get("status") == "running" for row in before_turns
            ),
            "held": "待发送" in screen,
            "frame": frame,
        }
        self.injections.append(record)
        self.result.check(
            "RECEIVED — Tab held %r for after the task" % text,
            record["held"],
            "待发送 row visible",
        )
        return record

    def inject_and_cancel(self, text: str, label: str):
        """Steer and cancel in one input batch.

        Both effects are queued in order (SteerCurrentTurn, then
        CancelCurrentTurn), which is the narrowest reproducible window between
        an accepted steer and the executor's next round.
        """
        before = len(self.proxy.requests())
        before_turns = turns(self.home)
        at = time.time()
        self.session.note("steer+cancel: " + text)
        self.session.pty.write(text + "\r\x03")
        time.sleep(0.8)
        frame = self.session.dump(label)
        self.injected.append(text)
        record = {
            "text": text,
            "lane": "explicit-steer-then-ctrlc",
            "at": at,
            "before_requests": before,
            "turn_running_at_inject": any(
                row.get("status") == "running" for row in before_turns
            ),
            "held": "待发送" in self.session.text(),
            "frame": frame,
        }
        self.injections.append(record)
        self.result.check(
            "RECEIVED — the composer accepted %r before the cancel" % text,
            text in self.session.text() or "待发送" in self.session.text(),
            "text visible in the TUI",
        )
        return record

    # -- evidence ---------------------------------------------------------

    def snapshot_diff(self, label: str) -> str:
        diff = subprocess.run(
            ["git", "diff", "--no-color"], cwd=self.repo, capture_output=True, text=True
        ).stdout
        status = subprocess.run(
            ["git", "status", "--porcelain"], cwd=self.repo, capture_output=True, text=True
        ).stdout
        with open(os.path.join(self.evidence, label + ".diff"), "w", encoding="utf-8") as fh:
            fh.write(diff)
        with open(os.path.join(self.evidence, label + "-status.txt"), "w", encoding="utf-8") as fh:
            fh.write(status)
        return diff

    def delivered(self, text: str, after: float = 0):
        return self.wait_request_with(text, after=after, timeout=1)

    def visible(self, text: str) -> bool:
        """Whether any frame this case captured showed the text.

        The live screen only holds the last viewport; a message the user saw an
        hour ago is no longer in it. Frames are the record of what was shown.
        """
        frames_dir = os.path.join(self.evidence, "frames")
        if os.path.isdir(frames_dir):
            for name in sorted(os.listdir(frames_dir)):
                if not name.endswith(".txt"):
                    continue
                with open(os.path.join(frames_dir, name), encoding="utf-8", errors="replace") as fh:
                    if text in fh.read():
                        return True
        return text in self.session.text()

    def visible_in_frames(self, text: str, labels: list[str]) -> bool:
        """Only frames captured by the current session count.

        CASE 6 reuses one evidence directory across the killed process and the
        resumed one; an old frame must not make a missing resumed transcript
        look visible.
        """
        for label in labels:
            path = os.path.join(self.evidence, "frames", label + ".txt")
            if os.path.exists(path):
                with open(path, encoding="utf-8", errors="replace") as fh:
                    if text in fh.read():
                        return True
        return text in self.session.text()

    def analyze_delivery(self, text: str, after: float) -> dict:
        record = self.wait_request_with(text, after=after, timeout=600)
        if record:
            self.result.check(
                "DELIVERED — %r reached a model request" % text,
                True,
                "seq=%d at %.2fs after injection" % (record["seq"], record["t"] - after),
            )
        else:
            self.result.check("DELIVERED — %r reached a model request" % text, False)
        return record or {}

    def analyze_persistence(self, text: str) -> list[dict]:
        rows = user_messages(self.home, text)
        self.result.check(
            "PERSISTED — %r is in session_messages" % text,
            bool(rows),
            "user rows=%d" % len(rows),
        )
        return rows

    def analyze_ack(self, text: str, delivered_t: float, words: list[str]) -> bool:
        rows = messages(self.home)
        found = []
        for row in rows:
            created = iso_epoch(row.get("created_at"))
            if created is None or created < delivered_t:
                continue
            payload = row.get("payload") or ""
            if row.get("id") in [item.get("id") for item in messages(self.home, text)]:
                continue
            if '"role":"assistant"' not in payload and '"role": "assistant"' not in payload:
                continue
            if any(word in payload for word in words):
                found.append(row)
        self.result.check(
            "ACKNOWLEDGED — an assistant message after delivery names the new work",
            bool(found),
            "rows=%d" % len(found),
        )
        return bool(found)

    def probe_and_tests(self) -> tuple[dict, dict]:
        probe = probe_project(self.repo, self.evidence)
        tests = run_project_tests(self.repo, self.evidence)
        return probe, tests


def new_case(name: str, args, repeat: int = 0) -> CaseRun:
    run = CaseRun(name, args, repeat)
    run.proxy.tag = {"case": name, "repeat": repeat}
    return run


# ---------------------------------------------------------------------------
# Cases
# ---------------------------------------------------------------------------


def case1(args, repeat: int) -> int:
    """Default Enter during a real tool call: an append to the SAME turn.

    The fixed product contract: ordinary Enter while the agent works must be
    received, journaled before the ACK, and delivered to the running agent at
    its next suitable model round. It must not silently become a second task.
    """
    run = new_case("case1", args, repeat)
    r = run.result
    try:
        run.session.spawn()
        r.check("boot paints a frame", "leveler" in run.session.text().lower())
        run.session.type_and_enter(BASE_PROMPT)
        r.check("the slow test started (sentinel file)", run.wait_marker(300))

        injection = run.inject_default("还有管理员管理也没有。", "1-enter-during-tool")
        r.check(
            "the append arrived while the turn was running",
            injection["turn_running_at_inject"],
            "turn running at injection",
        )
        # Durable before the client's ACK: the row must already exist now.
        journal = journal_rows(run.home, "还有管理员管理也没有。")
        r.check(
            "PERSISTED — the append is in session_inputs before its delivery",
            len(journal) == 1,
            "journal rows=%d states=%s"
            % (len(journal), [row.get("state") for row in journal]),
        )
        journal_id = journal[0]["id"] if journal else None
        diff_at_injection = run.snapshot_diff("2-at-injection")
        r.check(
            "the appended feature was not implemented before the append was received",
            "admin" not in diff_at_injection.lower(),
            "diff at injection mentions admin: %s" % ("admin" in diff_at_injection.lower()),
        )

        delivered = run.analyze_delivery("还有管理员管理也没有。", injection["at"])
        settled = run.wait_all_settled(timeout=900)
        r.check(
            "the turn and any follow-up settled",
            settled is not None and all(x.get("status") != "running" for x in settled),
            "turns=%s" % ([x.get("status") for x in settled] if settled else "none"),
        )
        turn1 = next(
            (row for row in turns(run.home) if row.get("status") != "running"),
            None,
        )
        if delivered and turn1:
            end_at = iso_epoch(turn1.get("finished_at"))
            r.check(
                "DELIVERED INSIDE THE RUNNING TASK — the append reached a model round before the turn ended",
                end_at is not None and delivered["t"] <= end_at + 2,
                "request t=%.3f turn1_end=%s"
                % (
                    delivered["t"],
                    "%.3f" % end_at if end_at else "none",
                ),
            )
            r.check(
                "the append is not a second task the user never asked for",
                len([row for row in turns(run.home) if row.get("status") != "running"]) == 1,
                "turns=%d" % len([row for row in turns(run.home) if row.get("status") != "running"]),
            )
        else:
            r.check(
                "DELIVERED INSIDE THE RUNNING TASK — the append reached a model round before the turn ended",
                False,
                "no delivered request or no terminal turn",
            )
        run.session.dump("3-after-turns")

        rows = user_messages(run.home, "还有管理员管理也没有。")
        r.check(
            "PERSISTED — exactly one transcript row for the append",
            len(rows) == 1,
            "user rows=%d" % len(rows),
        )
        final_journal = journal_rows(run.home, "还有管理员管理也没有。")
        r.check(
            "CONSUMED — the exact journal row is settled as consumed",
            len(final_journal) == 1
            and final_journal[0].get("state") == "consumed"
            and final_journal[0].get("id") == journal_id
            and bool(rows)
            and final_journal[0].get("ordinal") == rows[0].get("ordinal"),
            "state=%s id=%s journal_ordinal=%s transcript_ordinal=%s"
            % (
                final_journal[0].get("state") if final_journal else "none",
                final_journal[0].get("id") if final_journal else "none",
                final_journal[0].get("ordinal") if final_journal else "none",
                rows[0].get("ordinal") if rows else "none",
            ),
        )
        if delivered:
            run.analyze_ack("还有管理员管理也没有。", delivered["t"], ["管理员", "admin"])
        probe, tests = run.probe_and_tests()
        r.check("EXECUTED — an admin page exists", probe.get("admin_status") == 200, "path=%s status=%s" % (probe.get("admin_path"), probe.get("admin_status")))
        r.check("EXECUTED — the admin page lists Alice Chen", bool(probe.get("admin_has_alice")), str(probe.get("admin_has_alice")))
        r.check("EXECUTED — the project's own tests pass", tests.get("ok") is True, str(tests.get("ran")))
        r.check(
            "VISIBLE — the append row and its delivery state were painted",
            injection.get("delivery_state") and run.visible("还有管理员管理也没有。"),
            "delivery state=%s" % injection.get("delivery_state"),
        )
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case2(args, repeat: int) -> int:
    """Steer while a model request is in flight: delivered on the next round."""
    run = new_case("case2", args, repeat)
    r = run.result
    try:
        run.session.spawn()
        run.session.type_and_enter(BASE_PROMPT)
        prompt_at = time.time()
        inflight = run.wait_in_flight(min_age=0.8, timeout=240)
        r.check(
            "a model request was genuinely in flight at injection time",
            inflight is not None,
            "seq=%s" % (inflight and inflight["seq"]),
        )
        injection = run.inject_steer("账户列表还需要支持按状态筛选。", "1-steer-during-model")
        ended = None
        if inflight:
            deadline = time.time() + 30
            while time.time() < deadline:
                ended = next(
                    (
                        item
                        for item in run.proxy.ends()
                        if item.get("seq") == inflight["seq"]
                    ),
                    None,
                )
                if ended:
                    break
                time.sleep(0.05)
        r.check(
            "the injection landed inside the request interval",
            bool(inflight and ended and injection["at"] < ended["t"]),
            "inject=%.3f end=%s" % (
                injection["at"],
                "%.3f" % ended["t"] if ended else "not observed",
            ),
        )
        r.check(
            "the in-flight request could not contain the new text (already serialized)",
            bool(inflight)
            and not any(
                "按状态筛选" in item for item in inflight.get("user_texts", [])
            ),
        )
        delivered = run.analyze_delivery("账户列表还需要支持按状态筛选。", injection["at"])
        if inflight and delivered:
            r.check(
                "the first request containing it is later than the in-flight request",
                delivered["seq"] > inflight["seq"],
                "in_flight=%d delivered=%d" % (inflight["seq"], delivered["seq"]),
            )
        rows = run.wait_turns(1, timeout=600)
        r.check(
            "the steered turn settled",
            rows is not None and all(x.get("status") != "running" for x in rows),
            "turns=%s" % ([x.get("status") for x in rows] if rows else "none"),
        )
        run.session.dump("2-after-turn")
        run.analyze_persistence("账户列表还需要支持按状态筛选。")
        if delivered:
            run.analyze_ack("账户列表还需要支持按状态筛选。", delivered["t"], ["状态", "筛选", "status"])
        probe, tests = run.probe_and_tests()
        r.check("EXECUTED — /accounts?status=active filters", bool(probe.get("status_filter")), str(probe.get("status_filter")))
        r.check("EXECUTED — /accounts?status=disabled filters", bool(probe.get("status_filter_disabled")), str(probe.get("status_filter_disabled")))
        r.check("EXECUTED — the project's own tests pass", tests.get("ok") is True, str(tests.get("ran")))
        r.check("VISIBLE — the steer is painted as an admitted user row", run.visible("账户列表还需要支持按状态筛选"), injection["frame"])
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case3(args, repeat: int) -> int:
    """Three steers inside one tool window: order and completeness."""
    run = new_case("case3", args, repeat)
    r = run.result
    texts = [
        "增加管理员管理。",
        "管理员需要支持启用和禁用。",
        "管理员列表还需要搜索。",
    ]
    try:
        run.session.spawn()
        run.session.type_and_enter(BASE_PROMPT)
        prompt_at = time.time()
        r.check("the slow test started (sentinel file)", run.wait_marker(300))
        for index, text in enumerate(texts, 1):
            run.inject_steer(text, "1-steer-%d" % index)
            time.sleep(0.2)
        run.session.dump("2-three-steers-sent")
        deadline = time.time() + 600
        first = {}
        while time.time() < deadline and len(first) < len(texts):
            for record in run.proxy.requests(started_after=prompt_at):
                for text in texts:
                    if text in first:
                        continue
                    if any(text in item for item in record.get("user_texts", [])):
                        first[text] = record
            if all(text in first for text in texts):
                break
            time.sleep(0.1)
        rows = run.wait_turns(1, timeout=600)
        r.check(
            "the steered turn settled",
            rows is not None and all(x.get("status") != "running" for x in rows),
            "turns=%s" % ([x.get("status") for x in rows] if rows else "none"),
        )
        run.session.dump("3-after-turn")
        r.check(
            "DELIVERED — all three steers reached model requests",
            all(text in first for text in texts),
            "seqs=%s" % {text: first.get(text, {}).get("seq") for text in texts},
        )
        appearances = []
        for text in texts:
            record = first.get(text)
            if record is None:
                continue
            request = json.load(
                open(os.path.join(run.evidence, "requests", "%04d.json" % record["seq"]))
            )
            user_texts = [
                message.get("content")
                for message in request.get("messages", [])
                if message.get("role") == "user" and isinstance(message.get("content"), str)
            ]
            index = next(
                (i for i, item in enumerate(user_texts) if text in item), None
            )
            appearances.append((record["seq"], index if index is not None else -1, text))
        appearances.sort()
        r.check(
            "ORDER — the first appearances follow the order the user sent",
            [item[2] for item in appearances] == texts,
            str([(item[0], item[1], item[2]) for item in appearances]),
        )
        own_message = {}
        for text in texts:
            record = first.get(text)
            if record is None:
                own_message[text] = False
                continue
            request = json.load(
                open(os.path.join(run.evidence, "requests", "%04d.json" % record["seq"]))
            )
            matching = [
                message.get("content")
                for message in request.get("messages", [])
                if message.get("role") == "user"
                and isinstance(message.get("content"), str)
                and text in message.get("content")
            ]
            own_message[text] = len(matching) == 1 and matching[0].strip() == text
        r.check(
            "COMPLETENESS — each steer is its own exact message, not merged away",
            all(own_message.values()),
            str(own_message),
        )
        for text in texts:
            run.analyze_persistence(text)
        probe, tests = run.probe_and_tests()
        r.check("EXECUTED — an admin page exists", probe.get("admin_status") == 200, "path=%s status=%s" % (probe.get("admin_path"), probe.get("admin_status")))
        r.check("EXECUTED — the admin page lists Alice Chen", bool(probe.get("admin_has_alice")), str(probe.get("admin_has_alice")))
        r.check("EXECUTED — /admin exposes enable/disable affordance or state", bool(probe.get("admin_enable_disable_text")), str(probe.get("admin_enable_disable_text")))
        r.check("EXECUTED — /admin?q= filters the admin list", bool(probe.get("admin_search_supported")), str(probe.get("admin_search_supported")))
        r.check("EXECUTED — the project's own tests pass", tests.get("ok") is True, str(tests.get("ran")))
        r.check("VISIBLE — the three steers are in the live transcript", all(run.visible(text) for text in texts))
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case4(args, repeat: int) -> int:
    """An independent follow-up during tool execution."""
    run = new_case("case4", args, repeat)
    r = run.result
    text = "权限页面完成后，再给项目增加一个健康检查接口。"
    try:
        run.session.spawn()
        run.session.type_and_enter(BASE_PROMPT)
        prompt_at = time.time()
        r.check("the slow test started (sentinel file)", run.wait_marker(300))
        injection = run.inject_steer(text, "1-steer-independent")
        delivered = run.analyze_delivery(text, injection["at"])
        rows = run.wait_turns(1, timeout=900)
        r.check(
            "the steered turn settled",
            rows is not None and all(x.get("status") != "running" for x in rows),
            "turns=%s" % ([x.get("status") for x in rows] if rows else "none"),
        )
        run.session.dump("2-after-turn")
        run.analyze_persistence(text)
        if delivered:
            run.analyze_ack(text, delivered["t"], ["health", "健康", "健康检查"])
        probe, tests = run.probe_and_tests()
        r.check("EXECUTED — /health exists", probe.get("health_status") == 200, "status=%s" % probe.get("health_status"))
        r.check("EXECUTED — /health reports a healthy body", bool(probe.get("health_body")), str(probe.get("health_body"))[:120])
        r.check("EXECUTED — the project's own tests pass", tests.get("ok") is True, str(tests.get("ran")))
        r.check("VISIBLE — the independent follow-up is in the live transcript", run.visible(text))
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case5(args, repeat: int, lane: str) -> int:
    """Append while the turn's final model round may still be streaming.

    The driver cannot know which response is the last one before it closes, so
    it injects into a main request that has been in flight for a second and
    classifies afterwards. A no-tool response that is also the last main
    request of the turn is the turn-end window the case is named for.
    """
    run = new_case("case5" + lane, args, repeat)
    r = run.result
    text = "完成后再检查一次所有测试。"
    try:
        run.session.spawn()
        run.session.type_and_enter(BASE_PROMPT)
        prompt_at = time.time()
        # The base prompt runs the suite before and after the edits. The model
        # request carrying the second result is the final-response window; a
        # sleep is not evidence that the turn is near its end.
        inflight = run.wait_late_in_flight(runs=2, timeout=600)
        if inflight is None:
            inflight = run.wait_late_in_flight(runs=1, timeout=120)
            r.note("only one completed test run was observed before the window")
        r.check(
            "a main model request was in flight",
            inflight is not None,
            "seq=%s" % (inflight and inflight["seq"]),
        )
        if lane == "a":
            injection = run.inject_default(text, "1-near-end")
        else:
            injection = run.inject_steer(text, "1-near-end")
        end = None
        if inflight:
            deadline = time.time() + 120
            while time.time() < deadline:
                end = next(
                    (item for item in run.proxy.ends() if item.get("seq") == inflight["seq"]),
                    None,
                )
                if end:
                    break
                time.sleep(0.05)
        r.check(
            "the injection landed while that request was still open",
            bool(inflight and end and injection["at"] < end["t"]),
            "inject=%.3f end=%s" % (
                injection["at"],
                "%.3f" % end["t"] if end else "not observed",
            ),
        )
        no_tool = bool(end) and not end.get("saw_tool_call")
        r.note(
            "the request carried a tool call: %s; final-response window: %s"
            % (bool(end and end.get("saw_tool_call")), no_tool)
        )
        # A default Enter is held only if the TUI was still busy; a steer is
        # delivered either by the next round or by the turn-end flush.
        delivered = run.analyze_delivery(text, injection["at"])
        settled = run.wait_all_settled(timeout=900)
        r.check(
            "the turn and any follow-up settled",
            settled is not None and all(x.get("status") != "running" for x in settled),
            "turns=%d" % (len(settled) if settled else 0),
        )
        run.session.dump("2-after-follow-up")
        run.analyze_persistence(text)
        if delivered:
            run.analyze_ack(text, delivered["t"], ["测试", "test", "检查"])
            after = [
                record
                for record in run.proxy.requests(started_after=delivered["t"])
                if record["seq"] != delivered["seq"]
            ]
            reran = False
            for record in after[:3]:
                body = open(
                    os.path.join(run.evidence, record["body"]), encoding="utf-8"
                ).read()
                if "unittest" in body:
                    reran = True
                    break
            r.check(
                "EXECUTED — a later model round carries the test command for the follow-up",
                reran,
                "checked %d request bodies after delivery" % min(3, len(after)),
            )
        probe, tests = run.probe_and_tests()
        r.check("EXECUTED — the project still passes its own tests", tests.get("ok") is True, str(tests.get("ran")))
        rows_with_text = user_messages(run.home, text)
        r.check(
            "no duplicate user row for the same follow-up",
            len(rows_with_text) <= 1,
            "rows=%d" % len(rows_with_text),
        )
        r.check(
            "VISIBLE — the follow-up is in the live transcript",
            run.visible(text),
            "held as 待发送 at injection: %s" % injection["held"],
        )
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case5c(args, repeat: int) -> int:
    """A steer followed immediately by a user cancel: dropped, or resurrected?"""
    run = new_case("case5c", args, repeat)
    r = run.result
    text = "取消后请记住这条指令：增加管理员管理。"
    later = "只回复 OK，不要调用工具。"
    try:
        run.session.spawn()
        run.session.type_and_enter(BASE_PROMPT)
        prompt_at = time.time()
        r.check("the slow test started (sentinel file)", run.wait_marker(300))
        injection = run.inject_and_cancel(text, "1-steer-and-cancel")
        rows = run.wait_turns(1, timeout=300)
        turn1 = rows[0] if rows else None
        r.check(
            "the running turn ended after the cancel",
            turn1 is not None and turn1.get("status") != "running",
            "status=%s" % (turn1.get("status") if turn1 else "none"),
        )
        finished = (
            query(
                run.home,
                "SELECT payload FROM events WHERE turn_id=? AND type='turn_finished'",
                (turn1.get("id"),),
            )
            if turn1
            else []
        )
        outcome = ""
        if finished:
            try:
                outcome = (
                    json.loads(finished[-1]["payload"])
                    .get("payload", {})
                    .get("outcome", "")
                )
            except Exception:
                outcome = ""
        r.check(
            "the turn ended because the user cancelled it",
            outcome in ("cancelled", "interrupted"),
            "outcome=%s" % (outcome or "unknown"),
        )
        # If the cancel was processed before the steer, the steer handler sees
        # an idle runtime and falls back to SubmitMessage, which starts a new
        # turn. Give that path time to materialize, then classify by ownership.
        time.sleep(2.0)
        rows = turns(run.home)
        fallback = rows[1] if len(rows) > 1 else None
        if fallback:
            run.wait_all_settled(timeout=600)
            rows = turns(run.home)
        run.session.dump("2-after-cancel")
        persisted = user_messages(run.home, text)
        turn_by_id = {row.get("id"): row for row in rows}
        disposition = "dropped"
        if persisted:
            row = persisted[0]
            owner = turn_by_id.get(row.get("turn_id"))
            if fallback and owner and owner.get("id") == fallback.get("id"):
                disposition = "fallback-new-turn"
            elif turn1 and owner and owner.get("id") == turn1.get("id"):
                disposition = "consumed-before-cancel"
            else:
                disposition = "persisted-elsewhere"
        r.note(
            "steer disposition: %s; turn=%s; created_at=%s"
            % (
                disposition,
                persisted[0].get("turn_id") if persisted else "none",
                persisted[0].get("created_at") if persisted else "none",
            )
        )
        if disposition == "consumed-before-cancel":
            delivered = run.wait_request_with(text, after=prompt_at, timeout=5)
            r.check(
                "the already-consumed steer did reach the model once",
                delivered is not None,
                "seq=%s" % (delivered and delivered["seq"]),
            )
        elif disposition == "fallback-new-turn":
            delivered = run.wait_request_with(text, after=prompt_at, timeout=30)
            r.check(
                "DELIVERED — the steer ran as a new turn instead of being lost",
                delivered is not None,
                "seq=%s" % (delivered and delivered["seq"]),
            )
        elif disposition == "dropped":
            during = run.wait_request_with(text, after=prompt_at, timeout=1)
            r.check("the cancelled steer was not delivered before the cancel", during is None)
        else:
            r.check(
                "the persisted steer belongs to a known turn",
                False,
                "turn=%s" % (persisted[0].get("turn_id") if persisted else "none"),
            )
        time.sleep(1.0)
        run.session.type_and_enter(later)
        later_at = time.time()
        run.wait_all_settled(timeout=300)
        run.session.dump("3-after-later-turn")
        later_request = None
        deadline = time.time() + 30
        while time.time() < deadline and later_request is None:
            for record in run.proxy.requests(started_after=later_at):
                later_request = record
                break
            time.sleep(0.1)
        later_texts = later_request.get("user_texts") if later_request else []
        positions = {
            "steer": next((i for i, item in enumerate(later_texts) if text in item), None),
            "later": next((i for i, item in enumerate(later_texts) if later in item), None),
        }
        r.note("first later request user-message positions: %s" % positions)
        later_rows = user_messages(run.home, text)
        if disposition == "dropped":
            resurrected = (
                positions["steer"] is not None
                and positions["later"] is not None
                and positions["steer"] > positions["later"]
            )
            r.check(
                "the cancelled steer does not resurface in a later turn",
                not resurrected,
                "steer=%s later=%s rows=%d"
                % (positions["steer"], positions["later"], len(later_rows)),
            )
            r.check(
                "the cancelled steer is not persisted later either",
                not later_rows,
                "rows=%d" % len(later_rows),
            )
        else:
            r.check(
                "the consumed steer is context, not a second injection",
                positions["steer"] is None
                or positions["later"] is None
                or positions["steer"] < positions["later"],
                "steer=%s later=%s" % (positions["steer"], positions["later"]),
            )
            r.check(
                "the consumed steer was persisted exactly once",
                len(persisted) == 1,
                "rows=%d" % len(persisted),
            )
        # In both branches the person needs a visible disposition. The screen
        # still paints the admitted steer; the question is whether it is now
        # running, persisted, or explicitly discarded.
        r.check(
            "VISIBLE — the steer was painted as admitted",
            run.visible(text),
            injection.get("frame", ""),
        )
        if disposition == "dropped":
            r.check(
                "VISIBLE — the dropped steer is explicitly marked as not sent",
                any(run.visible(marker) for marker in ("未发送", "未处理", "已丢弃", "指令未")),
                "it was painted as admitted, so a silent drop would be misleading",
            )
        elif disposition == "consumed-before-cancel":
            r.check(
                "VISIBLE — the cancelled turn is shown as stopped",
                any(run.visible(marker) for marker in ("已停止", "已取消", "中断")),
                "the consumed steer is normal context; only the turn outcome needs a notice",
            )
        else:
            r.check(
                "VISIBLE — the fallback turn is visible",
                run.visible(text),
                "the steer became a new turn",
            )
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case5d(args, repeat: int) -> int:
    """A deterministic turn-end window: steer during a one-round answer.

    CASE 5A/5B inject into a long coding turn; whether that request is the
    turn's last one is only knowable after the fact. This supplementary run
    asks for a text-only answer, so the request in flight is the final round,
    and a steer admitted during it has no next round to consume it. That is the
    exact input the turn-end flush exists for.
    """
    run = new_case("case5d", args, repeat)
    r = run.result
    text = "完成后再检查一次所有测试。"
    try:
        run.session.spawn()
        run.session.type_and_enter(
            "只回复 OK，不要调用任何工具（包括 update_goal 和 update_plan）。回答后立即结束。"
        )
        inflight = run.wait_in_flight(min_age=0.5, timeout=120)
        r.check(
            "the one-round answer was in flight",
            inflight is not None,
            "seq=%s" % (inflight and inflight["seq"]),
        )
        injection = run.inject_steer(text, "1-final-window")
        boundary_deadline = time.time() + 120
        while time.time() < boundary_deadline:
            if len(turns(run.home)) >= 2:
                # Let the new turn's UserMessageAdded reach the transcript,
                # then capture the boundary before the answer scrolls it away.
                time.sleep(0.3)
                run.session.dump("2-flush-boundary")
                break
            time.sleep(0.05)
        rows = run.wait_all_settled(timeout=600)
        r.check(
            "the short turn and its follow-up settled",
            rows is not None and all(row.get("status") != "running" for row in rows),
            "turns=%s" % ([row.get("status") for row in rows] if rows else "none"),
        )
        short_turn = rows[0] if rows else None
        end = None
        if inflight:
            deadline = time.time() + 30
            while time.time() < deadline:
                end = next(
                    (item for item in run.proxy.ends() if item.get("seq") == inflight["seq"]),
                    None,
                )
                if end:
                    break
                time.sleep(0.05)
        r.note(
            "the one-round request carried a tool call: %s; final-response window: %s"
            % (bool(end and end.get("saw_tool_call")), bool(end) and not end.get("saw_tool_call"))
        )
        r.check(
            "the steer was not already in the in-flight request",
            bool(inflight) and not any(text in item for item in inflight.get("user_texts", [])),
            "the request body is snapshotted before it is sent",
        )
        delivered = run.wait_request_with(text, after=injection["at"], timeout=120)
        r.check(
            "DELIVERED — the steer reached a later model request",
            delivered is not None,
            "seq=%s" % (delivered and delivered["seq"]),
        )
        persisted = user_messages(run.home, text)
        r.check("PERSISTED — the steer is a user row", len(persisted) == 1, "rows=%d" % len(persisted))
        owner = persisted[0].get("turn_id") if persisted else None
        r.note(
            "steer disposition: owner turn=%s; short turn=%s; later turn=%s"
            % (
                owner,
                short_turn.get("id") if short_turn else "none",
                [row.get("id") for row in rows if short_turn and row.get("id") != short_turn.get("id")],
            )
        )
        run.session.dump("3-after-flush")
        # The row can land after the first post-flush frame; keep capturing
        # until a frame actually shows it, so "0 because it scrolled out" is
        # not reported as a dedupe failure.
        frames_dir = os.path.join(run.evidence, "frames")
        for attempt in range(20):
            shown = False
            for name in os.listdir(frames_dir) if os.path.isdir(frames_dir) else []:
                if not name.endswith(".txt"):
                    continue
                with open(
                    os.path.join(frames_dir, name), encoding="utf-8", errors="replace"
                ) as fh:
                    if text in fh.read():
                        shown = True
                        break
            if shown:
                break
            time.sleep(0.3)
            run.session.dump("4-flush-visible-%d" % attempt)
        counts = {}
        for name in sorted(os.listdir(frames_dir)) if os.path.isdir(frames_dir) else []:
            if not name.endswith(".txt"):
                continue
            with open(
                os.path.join(frames_dir, name), encoding="utf-8", errors="replace"
            ) as fh:
                counts[name] = sum(1 for line in fh if text in line)
        duplicate = max(counts.values(), default=0)
        r.check(
            "VISIBLE — the flushed steer is one user row, not duplicated",
            duplicate == 1,
            "max rows in one frame=%d, frames showing it=%d"
            % (duplicate, sum(1 for count in counts.values() if count)),
        )
        r.check(
            "VISIBLE — the steer is in the captured transcript",
            run.visible(text),
            "",
        )
        if delivered:
            run.analyze_ack(text, delivered["t"], ["测试", "test", "检查"])
        probe, tests = run.probe_and_tests()
        r.check(
            "EXECUTED — the follow-up ran the project's own tests",
            bool(tests.get("ran")),
            "ran=%s ok=%s (the fixture starts with test_search_by_email red)"
            % (tests.get("ran"), tests.get("ok")),
        )
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case5e(args, repeat: int) -> int:
    """Two unconsumed steers at turn end keep two message boundaries.

    CASE 5D proves the one-item flush. This variant puts two items into the
    same final request window: the turn ends with both still pending, so the
    flush must promote them as separate user messages — never one message with
    the two texts joined by a newline.
    """
    run = new_case("case5e", args, repeat)
    r = run.result
    first = "完成后再检查一次所有测试。"
    second = "另外告诉我这次一共运行了几次测试。"
    try:
        run.session.spawn()
        run.session.type_and_enter(
            "请用大约 200 个汉字说明为什么编写测试很重要，最后再回复一行 OK。"
            "不要调用任何工具（包括 update_goal 和 update_plan）。"
        )
        inflight = run.wait_in_flight(min_age=0.5, timeout=120)
        r.check(
            "the one-round answer was in flight",
            inflight is not None,
            "seq=%s" % (inflight and inflight["seq"]),
        )
        at = time.time()
        run.session.note("steer: " + first)
        run.session.pty.write(first + "\r")
        time.sleep(0.2)
        run.session.note("steer: " + second)
        run.session.pty.write(second + "\r")
        # Read the journal BEFORE the turn gets a chance to flush. The second
        # Enter needs one event-loop turn to reach the runtime, so wait for
        # its row instead of assuming the write already admitted it; the
        # longer streamed answer keeps the final window open meanwhile.
        deadline = time.time() + 5
        states = []
        still_running = False
        by_text = {}
        while time.time() < deadline:
            by_text = {row.get("content"): row for row in journal_rows(run.home)}
            states = [
                by_text.get(first, {}).get("state"),
                by_text.get(second, {}).get("state"),
            ]
            still_running = any(
                row.get("status") == "running" for row in turns(run.home)
            )
            if states == ["pending", "pending"]:
                break
            time.sleep(0.02)
        r.check(
            "RECEIVED — both appends are journaled before the turn ends",
            states == ["pending", "pending"] and still_running,
            "states=%s running=%s" % (states, still_running),
        )
        run.session.dump("1-both-admitted")
        boundary_deadline = time.time() + 120
        while time.time() < boundary_deadline:
            if len(turns(run.home)) >= 2:
                time.sleep(0.3)
                run.session.dump("2-flush-boundary")
                break
            time.sleep(0.05)
        rows = run.wait_all_settled(timeout=600)
        r.check(
            "the short turn and its follow-up settled",
            rows is not None and all(row.get("status") != "running" for row in rows),
            "turns=%s" % ([row.get("status") for row in rows] if rows else "none"),
        )
        run.session.dump("3-after-flush")

        delivered_first = run.wait_request_with(first, after=at, timeout=120)
        delivered_second = run.wait_request_with(second, after=at, timeout=120)
        r.check("DELIVERED — the first steer reached a model request", delivered_first is not None, "seq=%s" % (delivered_first and delivered_first["seq"]))
        r.check("DELIVERED — the second steer reached a model request", delivered_second is not None, "seq=%s" % (delivered_second and delivered_second["seq"]))

        first_rows = user_messages(run.home, first)
        second_rows = user_messages(run.home, second)
        r.check(
            "PERSISTED — each steer is exactly one transcript row",
            len(first_rows) == 1 and len(second_rows) == 1,
            "rows=%s" % ([len(first_rows), len(second_rows)]),
        )
        first_journal = journal_rows(run.home, first)
        second_journal = journal_rows(run.home, second)
        r.check(
            "PERSISTED — the two rows keep distinct identities",
            len(first_journal) == 1
            and len(second_journal) == 1
            and first_journal[0].get("id") != second_journal[0].get("id"),
            "ids=%s,%s"
            % (
                first_journal[0].get("id") if first_journal else None,
                second_journal[0].get("id") if second_journal else None,
            ),
        )

        # The first request that carries either text must carry BOTH as
        # separate user messages, in send order, with nothing joined.
        carrying = [
            record
            for record in run.proxy.requests(started_after=at)
            if any(
                first in text or second in text
                for text in record.get("user_texts", [])
            )
        ]
        user_texts = carrying[0].get("user_texts", []) if carrying else []
        joined = [text for text in user_texts if first in text and second in text]
        first_at = next((i for i, text in enumerate(user_texts) if first in text), None)
        second_at = next((i for i, text in enumerate(user_texts) if second in text), None)
        r.check(
            "BOUNDARY — the two steers are separate user messages, not joined",
            not joined
            and first_at is not None
            and second_at is not None
            and first_at < second_at,
            "joined=%d positions=%s texts=%s"
            % (len(joined), [first_at, second_at], user_texts[:8]),
        )

        frames_dir = os.path.join(run.evidence, "frames")
        counts = {}
        for name in sorted(os.listdir(frames_dir)) if os.path.isdir(frames_dir) else []:
            if not name.endswith(".txt"):
                continue
            with open(
                os.path.join(frames_dir, name), encoding="utf-8", errors="replace"
            ) as fh:
                lines = fh.readlines()
            counts[name] = (
                sum(1 for line in lines if first in line),
                sum(1 for line in lines if second in line),
                sum(1 for line in lines if first in line and second in line),
            )
        r.check(
            "VISIBLE — each flushed steer is one row, and no row merges them",
            counts
            and max(count[0] for count in counts.values()) <= 1
            and max(count[1] for count in counts.values()) <= 1
            and max(count[2] for count in counts.values()) == 0,
            "max rows=(%d,%d) merged=%d"
            % (
                max((count[0] for count in counts.values()), default=0),
                max((count[1] for count in counts.values()), default=0),
                max((count[2] for count in counts.values()), default=0),
            ),
        )
        final = {row.get("content"): row.get("state") for row in journal_rows(run.home)}
        r.check(
            "CONSUMED — both journal rows settled after delivery",
            final.get(first) == "consumed" and final.get(second) == "consumed",
            "states=%s" % [final.get(first), final.get(second)],
        )
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case6(args, repeat: int, consumed: bool) -> int:
    """SIGKILL a steer before/after the next round consumes it, then resume."""
    name = "case6" + ("b" if consumed else "a")
    run = new_case(name, args, repeat)
    r = run.result
    text = "增加管理员管理。"
    try:
        run.session.spawn()
        run.session.type_and_enter(BASE_PROMPT)
        prompt_at = time.time()
        r.check("the slow test started (sentinel file)", run.wait_marker(300))
        injection = run.inject_steer(
            text, "1-steer-before-kill", settle=0.25 if not consumed else 0.6
        )
        run.session_id = latest_session(run.home)
        if consumed:
            delivered = run.wait_request_with(text, after=injection["at"], timeout=300)
            r.check("the steer was consumed by a model request before the kill", delivered is not None, "seq=%s" % (delivered and delivered["seq"]))
        else:
            # The ACK is the durable row: wait for it, then kill immediately,
            # before the executor's next round can consume it.
            ack_deadline = time.time() + 15
            while time.time() < ack_deadline and not journal_rows(run.home, text):
                time.sleep(0.02)
        persisted_before = user_messages(run.home, text)
        journal_before = journal_rows(run.home, text)
        r.check(
            "PERSISTED — the append is journaled before the kill",
            len(journal_before) == 1,
            "journal rows=%d states=%s"
            % (len(journal_before), [row.get("state") for row in journal_before]),
        )
        if not consumed:
            r.check(
                "the steer had not been consumed before the kill",
                not persisted_before,
                "user rows=%d" % len(persisted_before),
            )
        r.note("persisted rows before kill: %d" % len(persisted_before))
        run.session.kill()
        if not run.session_id:
            r.check("the session id is available for resume", False)
            return 1
        r.check("the session row survived the kill", bool(query(run.home, "SELECT id FROM sessions WHERE id=?", (run.session_id,))))
        journal_after_kill = journal_rows(run.home, text)
        r.check(
            "the journal row survived the SIGKILL",
            len(journal_after_kill) == 1
            and journal_after_kill[0].get("state") == journal_before[0].get("state"),
            "state=%s" % (journal_after_kill[0].get("state") if journal_after_kill else "none"),
        )
        run.session = Session(run.env, run.repo, args.binary, run.evidence, run.log)
        run.session.proxy = run.proxy
        run.session.spawn(session=run.session_id)
        r.check("Resume reopened the real session", "leveler" in run.session.text().lower())
        run.session.dump("2-resumed")
        recovery = run.confirm_recovery()
        r.check(
            "the crash-recovery overlay was acknowledged before the next turn",
            recovery,
            "an unacknowledged unknown-outcome tool call blocks submission",
        )
        persisted_after = user_messages(run.home, text)
        r.check(
            "the steer's transcript state is unchanged by the crash",
            len(persisted_after) == len(persisted_before),
            "before=%d after=%d" % (len(persisted_before), len(persisted_after)),
        )
        # Drive one turn so a recovered but unconsumed row is handed to the
        # resumed agent. The journal is the authority, not an in-memory queue.
        resumed_at = time.time()
        run.session.type_and_enter("只回复 OK，不要调用工具。")
        deadline = time.time() + 300
        new_rows = []
        while time.time() < deadline:
            rows = turns(run.home)
            new_rows = [
                row
                for row in rows
                if (iso_epoch(row.get("created_at")) or 0) >= resumed_at - 2
            ]
            if new_rows and all(row.get("status") != "running" for row in new_rows):
                break
            time.sleep(0.25)
        r.check(
            "the resume turn settled",
            bool(new_rows) and all(row.get("status") != "running" for row in new_rows),
            "new turns=%s" % ([row.get("status") for row in new_rows] if new_rows else "none"),
        )
        run.session.dump("3-after-resume-turn")
        requests_after = [
            record
            for record in run.proxy.requests(started_after=resumed_at)
            if any(text in item for item in record.get("user_texts", []))
        ]
        if consumed:
            r.check(
                "the consumed steer is present in the resumed model context",
                bool(requests_after),
                "requests=%d" % len(requests_after),
            )
            for record in requests_after:
                count = sum(1 for item in record["user_texts"] if text in item)
                r.check(
                    "the consumed steer is not duplicated in request %d" % record["seq"],
                    count == 1,
                    "occurrences=%d" % count,
                )
            r.check(
                "VISIBLE after resume — the consumed steer is in the restored transcript",
                run.visible_in_frames(text, ["2-resumed", "3-after-resume-turn"]),
                "persisted before the kill",
            )
        else:
            r.check(
                "RECOVERED — the unconsumed append reaches the resumed model context",
                bool(requests_after),
                "requests containing it=%d" % len(requests_after),
            )
            for record in requests_after:
                count = sum(1 for item in record["user_texts"] if text in item)
                r.check(
                    "the recovered append is not duplicated in request %d" % record["seq"],
                    count == 1,
                    "occurrences=%d" % count,
                )
            recovered_rows = user_messages(run.home, text)
            r.check(
                "the recovered append is persisted exactly once",
                len(recovered_rows) == 1,
                "user rows=%d" % len(recovered_rows),
            )
            recovered_journal = journal_rows(run.home, text)
            r.check(
                "the journal row is settled as consumed after recovery",
                len(recovered_journal) == 1
                and recovered_journal[0].get("state") == "consumed",
                "state=%s" % (recovered_journal[0].get("state") if recovered_journal else "none"),
            )
            r.check(
                "VISIBLE after resume — the recovered append is in the transcript",
                run.visible_in_frames(text, ["2-resumed", "3-after-resume-turn"]),
                "the row was journaled before the kill",
            )
        r.check(
            "VISIBLE before the kill — the steer was painted as admitted",
            text in open(injection["frame"], encoding="utf-8").read() or "待发送" in open(injection["frame"], encoding="utf-8").read(),
            injection["frame"],
        )
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case7(args, repeat: int) -> int:
    """One complex build-out with several related and unrelated steers."""
    run = new_case("case7", args, repeat)
    r = run.result
    try:
        run.session.spawn()
        prompt = (
            "把 PermHub 完善为可用的权限管理应用：账户页支持按姓名和邮箱搜索，"
            "角色页展示权限，管理员页支持列表、搜索和启用/禁用；修复现有失败测试，"
            "最后运行全部测试并通过。实际验证用测试或一次性脚本，"
            "不要启动长期运行的服务，也不要 wait_task 一个服务器。"
            "不要只回复说明，要实际修改和验证。"
        )
        run.session.type_and_enter(prompt)
        prompt_at = time.time()
        r.check("the slow test started (sentinel file)", run.wait_marker(300))
        run.inject_steer("另外，账户列表要支持按状态筛选。", "1-steer-status")
        inflight = run.wait_in_flight(min_age=0.6, timeout=300)
        r.check("a later model request was in flight", inflight is not None, "seq=%s" % (inflight and inflight["seq"]))
        run.inject_steer("管理员列表也要支持搜索。", "2-steer-admin-search")
        inflight2 = run.wait_in_flight(min_age=0.6, timeout=300)
        r.check("a third injection window was available", inflight2 is not None, "seq=%s" % (inflight2 and inflight2["seq"]))
        run.inject_steer("再增加一个 /health 健康检查接口。", "3-steer-health")
        texts = [
            "另外，账户列表要支持按状态筛选。",
            "管理员列表也要支持搜索。",
            "再增加一个 /health 健康检查接口。",
        ]
        rows = run.wait_all_settled(timeout=1200)
        r.check(
            "the complex turn settled",
            rows is not None and all(x.get("status") != "running" for x in rows),
            "turns=%s" % ([x.get("status") for x in rows] if rows else "none"),
        )
        run.session.dump("4-after-complex-turn")
        first = {}
        for record in run.proxy.requests(started_after=prompt_at):
            for text in texts:
                if text in first:
                    continue
                if any(text in item for item in record.get("user_texts", [])):
                    first[text] = record
        r.check(
            "DELIVERED — all three complex steers reached model requests",
            all(text in first for text in texts),
            "seqs=%s" % {text: first.get(text, {}).get("seq") for text in texts},
        )
        appearances = []
        for text in texts:
            record = first.get(text)
            if record is None:
                continue
            request = json.load(
                open(os.path.join(run.evidence, "requests", "%04d.json" % record["seq"]))
            )
            user_texts = [
                message.get("content")
                for message in request.get("messages", [])
                if message.get("role") == "user" and isinstance(message.get("content"), str)
            ]
            position = next((i for i, item in enumerate(user_texts) if text in item), -1)
            appearances.append((record["seq"], position, text))
        appearances.sort()
        r.check(
            "ORDER — the first appearances follow the step the user sent",
            [item[2] for item in appearances] == texts,
            str([(item[0], item[1], item[2]) for item in appearances]),
        )
        for text, words in zip(
            texts,
            (["状态", "筛选", "status"], ["管理员", "搜索", "admin"], ["health", "健康"]),
        ):
            if text in first:
                run.analyze_ack(text, first[text]["t"], words)
        for text in texts:
            run.analyze_persistence(text)
        probe, tests = run.probe_and_tests()
        r.check("EXECUTED — /accounts status filter", bool(probe.get("status_filter")), str(probe.get("status_filter")))
        r.check("EXECUTED — an admin page exists", probe.get("admin_status") == 200, "path=%s status=%s" % (probe.get("admin_path"), probe.get("admin_status")))
        r.check("EXECUTED — /admin search", bool(probe.get("admin_search_supported")), str(probe.get("admin_search_supported")))
        r.check("EXECUTED — /health exists", probe.get("health_status") == 200, "status=%s" % probe.get("health_status"))
        r.check("EXECUTED — the project's own tests pass", tests.get("ok") is True, str(tests.get("ran")))
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


def case8(args, repeat: int) -> int:
    """Tab is the explicit 'process after this task' lane.

    It must not steer the running turn, and it must not be lost: when the task
    settles, the held item starts its own turn with the same text.
    """
    run = new_case("case8", args, repeat)
    r = run.result
    text = "完成后告诉我这次一共运行了几次测试。"
    try:
        run.session.spawn()
        run.session.type_and_enter(BASE_PROMPT)
        r.check("the slow test started (sentinel file)", run.wait_marker(300))
        injection = run.inject_tab_lane(text, "1-tab-after-turn")
        early = run.wait_request_with(text, after=injection["at"], timeout=5)
        r.check(
            "the held item is NOT delivered to the running turn",
            early is None,
            "seq=%s" % (early and early["seq"]),
        )
        r.check(
            "the held item is not journaled as an accepted append",
            not journal_rows(run.home, text),
            "journal rows=%d" % len(journal_rows(run.home, text)),
        )
        rows = run.wait_all_settled(timeout=900)
        r.check(
            "the task and the after-task turn settled",
            rows is not None
            and all(row.get("status") != "running" for row in rows)
            and len(rows) >= 2,
            "turns=%d" % (len(rows) if rows else 0),
        )
        delivered = run.analyze_delivery(text, injection["at"])
        run.session.dump("2-after-task")
        persisted = user_messages(run.home, text)
        r.check(
            "PERSISTED — the held item is one transcript row",
            len(persisted) == 1,
            "user rows=%d" % len(persisted),
        )
        # A held item starts its own turn; it is not an append to the running
        # one. Its durable acceptance is the write-ahead turn row (written
        # before the runtime acknowledges the submission), not a
        # `session_inputs` row — that journal exists for input admitted
        # against a turn that is already running.
        initiating = query(
            run.home,
            "SELECT id, status, payload FROM turns WHERE payload LIKE ?",
            ("%" + text + "%",),
        )
        r.check(
            "DURABLE — the held item is the initiating message of its own turn",
            len(initiating) == 1
            and text in (initiating[0].get("payload") or "")
            and initiating[0].get("status") != "running",
            "turn rows=%s" % [row.get("id") for row in initiating],
        )
        r.check(
            "VISIBLE — the held item is in the transcript",
            run.visible(text),
            injection["frame"],
        )
    finally:
        run.finish()
    return 0 if all(c["ok"] is not False for c in r.checks) else 1


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "case",
        choices=[
            "pilot",
            "case1",
            "case2",
            "case3",
            "case4",
            "case5a",
            "case5b",
            "case5c",
            "case5d",
            "case5e",
            "case6a",
            "case6b",
            "case7",
            "case8",
            "all",
        ],
    )
    parser.add_argument("--binary", default=DEFAULT_BIN)
    parser.add_argument("--repeat", type=int, default=1)
    args = parser.parse_args()

    runners = {
        "case1": lambda repeat: case1(args, repeat),
        "case2": lambda repeat: case2(args, repeat),
        "case3": lambda repeat: case3(args, repeat),
        "case4": lambda repeat: case4(args, repeat),
        "case5a": lambda repeat: case5(args, repeat, "a"),
        "case5b": lambda repeat: case5(args, repeat, "b"),
        "case5c": lambda repeat: case5c(args, repeat),
        "case5d": lambda repeat: case5d(args, repeat),
        "case5e": lambda repeat: case5e(args, repeat),
        "case6a": lambda repeat: case6(args, repeat, consumed=False),
        "case6b": lambda repeat: case6(args, repeat, consumed=True),
        "case7": lambda repeat: case7(args, repeat),
        "case8": lambda repeat: case8(args, repeat),
    }
    if args.case == "pilot":
        args.case = "case1"
        return case1(args, 0)
    if args.case == "all":
        names = [name for name in runners if name not in ("case5a", "case5b")]
        worst = 0
        for name in names:
            for repeat in range(args.repeat):
                print("\n===== %s repeat %d =====" % (name, repeat + 1))
                code = runners[name](repeat)
                worst = max(worst, code)
        return worst
    for repeat in range(args.repeat):
        code = runners[args.case](repeat)
        if code != 0:
            return code
    return 0


if __name__ == "__main__":
    sys.exit(main())
