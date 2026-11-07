#!/usr/bin/env python3
"""Phase 3: appended instructions under a real daemon and two real TUIs.

Phase 2 proved the journal/delivery contract with `--in-process` TUIs. This
harness removes that stand-in: one `leveler serve` process owns the runtime and
two independent `leveler tui --session ...` processes attach to the same
session over the Unix socket. A recording proxy in front of the configured
DeepSeek gateway keeps proving what the provider actually received.

Evidence is written under ~/CodeLeveler-append-instruction-evidence/<unique-run>/:
  result.json / steps.log       assertions and timeline
  frames/<client>-<label>.txt   decoded screen of each TUI process
  requests/*.json               exact model request bodies
  proxy.jsonl                   request start/end, compaction and injected faults
  web-snapshots/*.json          the daemon's own snapshot, a third observer
  database.json                 SQLite read-back (journal, messages, turns)
  sessions.sqlite3              pre-cleanup SQLite checkpoint, including WAL
  daemon-*.log / web-*.log      process logs
  workspace.diff                resulting repository diff

Usage:
    python3 scripts/append_instruction_phase3.py sc1
    python3 scripts/append_instruction_phase3.py sc2
    python3 scripts/append_instruction_phase3.py all

Set LEVELER_APPEND_EVIDENCE_ROOT to choose a persistent archive directory and
LEVELER_BIN to select the binary under test. Runs never reuse evidence paths.
"""

from __future__ import annotations

import argparse
import datetime
import importlib.util
import json
import os
import re
import secrets
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "debug", "leveler")

_spec = importlib.util.spec_from_file_location(
    "append_instruction_dogfood", os.path.join(ROOT, "scripts", "append_instruction_dogfood.py")
)
dog = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(dog)

SLOW_PROMPT = (
    "请运行 python3 -m unittest tests.test_00_slow -v"
    "（调用命令时设置 timeout_seconds=120），等它结束，然后只汇报测试是否全部通过。"
    "不要修改任何文件。"
)
STEER_COUNT = (
    "追加要求：请在最终回答里写明 tests/test_00_slow.py 的循环一共会发出多少次 HTTP 请求"
    "（先读该文件确认，不要猜）。"
)


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_until(predicate, timeout: float, interval: float = 0.2):
    end = time.time() + timeout
    while time.time() < end:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    return None


def epoch_seconds(stamp: str | None) -> float | None:
    """Parse a storage timestamp (`...Z` / `...+00:00`) into epoch seconds."""
    if not stamp:
        return None
    try:
        return datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def body_text(message) -> str:
    content = message.get("content") if isinstance(message, dict) else None
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(
            part.get("text") or "" for part in content if isinstance(part, dict)
        )
    return ""


def decoded_user_texts(row: dict) -> list[str]:
    """Text segments of a user-role transcript row, read from its JSON payload.

    The serialized payload escapes quotes, newlines and backslashes, so looking
    for the original text inside it is not a transcript check. Decode first and
    match whole text segments; a malformed or non-user row contributes nothing.
    """
    try:
        payload = json.loads(row.get("payload") or "{}")
    except (TypeError, ValueError):
        return []
    if not isinstance(payload, dict) or payload.get("role") != "user":
        return []
    content = payload.get("content")
    if isinstance(content, str):
        return [content]
    if not isinstance(content, list):
        return []
    return [
        part["text"]
        for part in content
        if isinstance(part, dict)
        and part.get("type") == "text"
        and isinstance(part.get("text"), str)
    ]


def user_message_rows_in(rows: list[dict], text: str) -> list[dict]:
    """Rows whose decoded user text is exactly `text`."""
    return [row for row in rows if text in decoded_user_texts(row)]


class Phase3Proxy(dog.RecordingProxy):
    """The Phase 2 recording proxy plus compaction visibility and faults."""

    def __init__(self, evidence_dir: str, upstream_base: str):
        super().__init__(evidence_dir, upstream_base)
        self.compaction_delay = 0.0
        self.compaction_starts: list[float] = []
        self.fail_remaining = 0
        self.fail_delay = 0.0
        self.fail_status = 500
        self.fail_armed_at = 0.0
        self.fail_count = 0

    def fail_next(self, count: int, delay: float = 0.0, status: int = 500):
        with self.lock:
            self.fail_remaining = count
            self.fail_delay = delay
            self.fail_status = status
            self.fail_armed_at = time.time()
        self._write(
            {
                "kind": "fault_armed",
                "t": self.fail_armed_at,
                "count": count,
                "delay": delay,
                "status": status,
            }
        )

    def handle(self, handler, body: bytes):
        text = body.decode("utf-8", "replace")
        if "CONTEXT CHECKPOINT COMPACTION" in text:
            with self.lock:
                self.compaction_starts.append(time.time())
            self._write(
                {
                    "kind": "compaction_start",
                    "t": time.time(),
                    "seq_hint": self.seq + 1,
                }
            )
            if self.compaction_delay:
                time.sleep(self.compaction_delay)
        try:
            request = json.loads(text)
        except Exception:
            request = {}
        # Auxiliary calls (prompt suggestion, away summary, compaction) are
        # not the turn's own model round. A fault aimed at the turn must not
        # land on one of them just because it happened to be next.
        is_main = bool(request.get("tools"))
        with self.lock:
            fail = self.fail_remaining > 0 and is_main
            if fail:
                self.fail_remaining -= 1
                self.fail_count += 1
                delay = self.fail_delay
                status = self.fail_status
            else:
                delay = 0.0
                status = 500
        if not fail:
            super().handle(handler, body)
            return
        # A real HTTP failure, recorded exactly like a forwarded request so the
        # request body and the injected status are both evidence.
        messages = request.get("messages") or []
        with self.lock:
            self.seq += 1
            seq = self.seq
            tag = dict(self.tag)
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
                "model": request.get("model"),
                "message_count": len(messages),
                "user_texts": [
                    body_text(m) for m in messages if m.get("role") == "user"
                ],
                "tool_names": [
                    (tool.get("function") or {}).get("name") or tool.get("name")
                    for tool in (request.get("tools") or [])
                ],
                "body": os.path.relpath(body_path, os.path.dirname(self.path)),
                "fault": "http_%d" % status,
                "main": is_main,
            }
        )
        if delay:
            time.sleep(delay)
        try:
            handler.send_response(status)
            handler.send_header("Content-Type", "application/json")
            handler.send_header("Connection", "close")
            handler.end_headers()
            handler.wfile.write(
                b'{"error":{"message":"injected phase3 fault","type":"invalid_request_error"}}'
            )
            handler.wfile.flush()
        except Exception:
            pass
        handler.close_connection = True
        self._write(
            {
                "kind": "request_end",
                "seq": seq,
                "t": time.time(),
                "duration": time.time() - started,
                "status": status,
                "bytes": 0,
                "finish_reasons": [],
                "saw_tool_call": False,
                "error": None,
                "fault": "http_%d" % status,
            }
        )


class Tui:
    def __init__(self, name: str, env: dict, repo: str, evidence: str, log):
        self.name = name
        self.env = env
        self.repo = repo
        self.evidence = evidence
        self.frames = os.path.join(evidence, "frames")
        os.makedirs(self.frames, exist_ok=True)
        self.log = log
        self.pty_module = dog.load_pty_module()
        self.pty = None
        self.argv: list[str] = []

    def note(self, text: str):
        line = "[%s] %s %s" % (time.strftime("%H:%M:%S"), self.name, text)
        print(line)
        self.log.write(line + "\n")
        self.log.flush()

    def spawn(self, session: str | None = None):
        self.argv = [
            BIN,
            "--repo",
            self.repo,
            "tui",
            "--auto-approve",
            "--permission",
            "full-access",
        ]
        if session:
            self.argv += ["--session", session]
        self.note("spawn: " + " ".join(self.argv))
        self.pty = self.pty_module.Pty(self.argv, self.env, 120, 48)
        self.pty.wait_for("CodeLeveler", timeout=60)
        self.pty.wait_stable(1.5, 60)
        self.dump("boot")

    def dump(self, label: str) -> str:
        path = os.path.join(self.frames, "%s-%s.txt" % (self.name, label))
        with open(path, "w", encoding="utf-8") as fh:
            fh.write(self.pty.screen.text())
            fh.write("\n")
        raw = os.path.join(self.frames, "%s-%s.raw" % (self.name, label))
        with open(raw, "wb") as fh:
            fh.write(bytes(self.pty.raw))
        return path

    def text(self) -> str:
        return self.pty.screen.text()

    def wait_for(self, needle: str, timeout: float) -> bool:
        return self.pty.wait_for(needle, timeout)

    def type_and_enter(self, text: str, settle: float = 0.4):
        self.note("type: " + text)
        self.pty.write(text)
        time.sleep(settle)
        self.pty.key("\r")

    def key(self, *seqs: str):
        self.pty.key(*seqs)

    def kill(self):
        self.note("SIGKILL pid %d" % self.pty.proc.pid)
        try:
            os.killpg(os.getpgid(self.pty.proc.pid), signal.SIGKILL)
        except Exception as exc:
            self.note("kill failed: %s" % exc)
        time.sleep(1.0)
        self.pty.close()

    def quit(self):
        if self.pty and self.pty.alive():
            self.pty.key("\x03")
            time.sleep(0.6)
            self.pty.key("\x03")
            deadline = time.time() + 20
            while time.time() < deadline and self.pty.alive():
                time.sleep(0.2)
            self.pty.close()
            self.note("quit")


class Case:
    def __init__(self, name: str, fixture: str | None = None, model_limits: dict | None = None):
        self.name = name
        self.dir = dog.create_evidence_dir("phase3-" + name)
        dog.write_evidence_identity(self.dir, BIN)
        self.result = dog.Result(name)
        self.log = open(os.path.join(self.dir, "steps.log"), "w", encoding="utf-8")
        self.proxy = Phase3Proxy(self.dir, dog.upstream_base())
        old_fixture = dog.FIXTURE
        if fixture:
            dog.FIXTURE = fixture
        try:
            self.root, self.home, self.repo, self.env = dog.build_workspace(self.proxy.base_url())
        finally:
            dog.FIXTURE = old_fixture
        if model_limits:
            rewrite_model_limits(os.path.join(self.home, "config.toml"), model_limits)
        self.token = secrets.token_hex(16)
        self.env = dict(self.env)
        self.env["LEVELER_DAEMON_TOKEN"] = self.token
        self.daemon: subprocess.Popen | None = None
        self.daemon_logs: list = []
        self.daemon_port = 0
        self.replacement_runtimes: list[int] = []
        self.web: subprocess.Popen | None = None
        self.web_log = None
        self.web_port = 0
        self.tuis: list[Tui] = []
        self.session_id: str | None = None
        self.result.note("workspace: " + self.repo)
        self.result.note("evidence: " + self.dir)
        self.result.note("proxy: " + self.proxy.base_url())

    # -- process management ------------------------------------------------

    def _note(self, text: str):
        line = "[%s] %s" % (time.strftime("%H:%M:%S"), text)
        print(line)
        self.log.write(line + "\n")
        self.log.flush()

    def start_daemon(self, wait: float = 60) -> bool:
        self.daemon_port = free_port()
        log_path = os.path.join(self.dir, "daemon-%d.log" % (len(self.daemon_logs) + 1))
        log = open(log_path, "a", encoding="utf-8")
        self.daemon_logs.append(log)
        argv = [
            BIN,
            "--repo",
            self.repo,
            "serve",
            "--auto-approve",
            "--permission",
            "full-access",
            "--tcp",
            "127.0.0.1:%d" % self.daemon_port,
        ]
        self._note("daemon: " + " ".join(argv))
        self.daemon = subprocess.Popen(
            argv,
            env=self.env,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        def tcp_ready():
            try:
                with socket.create_connection(("127.0.0.1", self.daemon_port), timeout=0.5):
                    return True
            except Exception:
                return False

        ready = wait_until(
            lambda: self.daemon.poll() is None
            and tcp_ready()
            and dog.session_database(self.home) is not None,
            wait,
        )
        self._note("daemon pid=%d port=%d ready=%s" % (self.daemon.pid, self.daemon_port, bool(ready)))
        return bool(ready)

    def daemon_socket(self) -> str | None:
        base = os.path.join(self.home, "run", "sockets")
        try:
            names = [name for name in os.listdir(base) if name.endswith(".sock")]
        except FileNotFoundError:
            return None
        return os.path.join(base, names[0]) if names else None

    def replacement_runtime(self, parent_pid: int) -> dict | None:
        """The runtime a live TUI spawns after its daemon disappears.

        `ensure_default_runtime` starts a child `leveler ... serve --ready-json`
        for the same repo, and that process performs the startup recovery the
        dead daemon could not. Identify it as a real OS process instead of
        assuming the client merely reconnected to something.
        """
        try:
            out = subprocess.run(
                ["ps", "-axo", "pid=,ppid=,command="],
                capture_output=True,
                text=True,
                check=False,
            ).stdout
        except Exception:
            return None
        for line in out.splitlines():
            parts = line.strip().split(None, 2)
            if len(parts) != 3:
                continue
            pid, ppid, command = parts
            if (
                ppid == str(parent_pid)
                and " serve " in command
                and "--ready-json" in command
                and (
                    self.repo in command
                    or os.path.realpath(self.repo) in command
                )
            ):
                found = {"pid": int(pid), "ppid": int(ppid), "command": command}
                if found["pid"] not in self.replacement_runtimes:
                    self.replacement_runtimes.append(found["pid"])
                return found
        return None

    def kill_daemon(self):
        if not self.daemon:
            return
        pid = self.daemon.pid
        self._note("SIGKILL daemon group pid=%d" % pid)
        try:
            os.killpg(os.getpgid(pid), signal.SIGKILL)
        except Exception as exc:
            self._note("daemon kill failed: %s" % exc)
        deadline = time.time() + 10
        while time.time() < deadline and self.daemon.poll() is None:
            time.sleep(0.1)
        self._note("daemon poll after kill: %s" % self.daemon.poll())

    def stop_daemon(self):
        if not self.daemon:
            return
        if self.daemon.poll() is None:
            try:
                os.killpg(os.getpgid(self.daemon.pid), signal.SIGTERM)
            except Exception:
                pass
            try:
                self.daemon.wait(10)
            except Exception:
                try:
                    os.killpg(os.getpgid(self.daemon.pid), signal.SIGKILL)
                except Exception:
                    pass
        self.daemon = None

    def start_web(self, wait: float = 30) -> bool:
        self.web_port = free_port()
        log_path = os.path.join(self.dir, "web.log")
        self.web_log = open(log_path, "a", encoding="utf-8")
        argv = [
            BIN,
            "--repo",
            self.repo,
            "web",
            "--connect",
            "127.0.0.1:%d" % self.daemon_port,
            "--token",
            self.token,
            "--addr",
            "127.0.0.1:%d" % self.web_port,
        ]
        logged_argv = list(argv)
        logged_argv[logged_argv.index("--token") + 1] = "[REDACTED]"
        self._note("web: " + " ".join(logged_argv))
        self.web = subprocess.Popen(
            argv,
            env=self.env,
            stdout=self.web_log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )

        def healthy():
            try:
                with urllib.request.urlopen(
                    "http://127.0.0.1:%d/api/health?token=%s" % (self.web_port, self.token),
                    timeout=1,
                ) as response:
                    return response.status == 200
            except Exception:
                return False

        return bool(wait_until(healthy, wait))

    def stop_web(self):
        if self.web and self.web.poll() is None:
            try:
                os.killpg(os.getpgid(self.web.pid), signal.SIGTERM)
            except Exception:
                pass
            try:
                self.web.wait(5)
            except Exception:
                try:
                    os.killpg(os.getpgid(self.web.pid), signal.SIGKILL)
                except Exception:
                    pass
        self.web = None

    def tui(self, name: str) -> Tui:
        tui = Tui(name, self.env, self.repo, self.dir, self.log)
        self.tuis.append(tui)
        return tui

    def wait_session(self, timeout: float = 60) -> str | None:
        found = wait_until(lambda: dog.latest_session(self.home), timeout)
        self.session_id = found
        return found

    # -- observation -------------------------------------------------------

    def snapshot(self, label: str) -> dict | None:
        url = "http://127.0.0.1:%d/api/sessions/%s/snapshot?token=%s" % (
            self.web_port,
            self.session_id,
            self.token,
        )
        try:
            with urllib.request.urlopen(url, timeout=20) as response:
                data = json.loads(response.read().decode("utf-8"))
        except Exception as exc:
            self._note("snapshot %s failed: %s" % (label, exc))
            return None
        path = os.path.join(self.dir, "web-snapshots", label + ".json")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as fh:
            json.dump(data, fh, ensure_ascii=False, indent=1)
        return data

    def requests(self, after: float = 0) -> list[dict]:
        return self.proxy.requests(started_after=after or None)

    def request_bodies(self, after: float = 0) -> list[tuple[dict, dict]]:
        out = []
        for record in self.requests(after):
            path = os.path.join(self.dir, record["body"])
            try:
                body = json.load(open(path, encoding="utf-8"))
            except Exception:
                continue
            out.append((record, body))
        return out

    def occurrences(self, text: str, after: float = 0) -> list[dict]:
        """How many times each request's messages contain `text`."""
        out = []
        for record, body in self.request_bodies(after):
            count = sum(
                body_text(message).count(text) for message in (body.get("messages") or [])
            )
            out.append({"seq": record["seq"], "t": record["t"], "count": count})
        return out

    def occurrences_outside_objective_pin(self, text: str, after: float = 0) -> list[dict]:
        """Count `text` in every message EXCEPT the host-pinned objective.

        A steer that is promoted into a new turn becomes that turn's objective,
        and every turn pins its objective verbatim. Counting that pin as a
        second delivery would flag the runtime's normal objective pin as
        duplication; this counts the actual conversation messages.
        """
        out = []
        for record, body in self.request_bodies(after):
            count = 0
            for message in body.get("messages") or []:
                value = body_text(message)
                if value.lstrip().startswith("[Active objective — host-pinned]"):
                    continue
                count += value.count(text)
            out.append({"seq": record["seq"], "t": record["t"], "count": count})
        return out

    def wait_request_with(self, text: str, after: float = 0, timeout: float = 600):
        return wait_until(
            lambda: next(
                (row for row in self.occurrences(text, after) if row["count"] > 0), None
            ),
            timeout,
            0.5,
        )

    def journal(self, text: str) -> list[dict]:
        return dog.journal_rows(self.home, text)

    def journal_all(self) -> list[dict]:
        return dog.journal_rows(self.home)

    def proxy_records(self) -> list[dict]:
        records = []
        path = os.path.join(self.dir, "proxy.jsonl")
        try:
            with open(path, encoding="utf-8") as fh:
                for line in fh:
                    try:
                        records.append(json.loads(line))
                    except Exception:
                        continue
        except FileNotFoundError:
            pass
        return records

    def compaction_spans(self) -> list[dict]:
        """The wall-clock window each compaction request occupied."""
        records = self.proxy_records()
        spans = []
        for index, record in enumerate(records):
            if record.get("kind") != "compaction_start":
                continue
            seq = None
            end = None
            for later in records[index + 1 :]:
                if later.get("kind") == "request_start" and seq is None:
                    seq = later.get("seq")
                if (
                    seq is not None
                    and later.get("kind") == "request_end"
                    and later.get("seq") == seq
                ):
                    end = later.get("t")
                    break
            spans.append({"start": record.get("t"), "end": end, "seq": seq})
        return spans

    def compaction_requests(self) -> list[tuple[dict, dict]]:
        out = []
        for record, body in self.request_bodies():
            if "CONTEXT CHECKPOINT COMPACTION" in json.dumps(body, ensure_ascii=False):
                out.append((record, body))
        return out

    def body_contains(self, seq: int, text: str) -> bool:
        for record, body in self.request_bodies():
            if record.get("seq") == seq:
                return any(
                    text in body_text(message)
                    for message in (body.get("messages") or [])
                )
        return False

    def model_request_kinds(self) -> list[dict]:
        return dog.query(
            self.home,
            "SELECT kind, finish_reason, error_kind, created_at FROM model_requests "
            "ORDER BY created_at",
        )

    def tool_started(self, name: str, after_iso: str | None = None) -> list[dict]:
        rows = dog.query(
            self.home,
            "SELECT sequence, type, payload, created_at FROM events "
            "WHERE type = 'tool_call_started' ORDER BY sequence",
        )
        out = []
        for row in rows:
            payload = row.get("payload") or ""
            if '"name":"%s"' % name not in payload:
                continue
            if after_iso and (row.get("created_at") or "") <= after_iso:
                continue
            out.append(row)
        return out

    def message_rows(self, text: str) -> list[dict]:
        rows = dog.query(
            self.home,
            "SELECT id, ordinal, turn_id, input_id, payload FROM session_messages ORDER BY ordinal",
        )
        return [row for row in rows if text in (row.get("payload") or "")]

    def user_message_rows(self, text: str) -> list[dict]:
        return user_message_rows_in(dog.messages(self.home), text)

    def assistant_texts(self) -> list[str]:
        out = []
        for row in dog.messages(self.home):
            if dog.payload_role(row) != "assistant":
                continue
            try:
                payload = json.loads(row.get("payload") or "{}")
            except Exception:
                continue
            text = body_text(payload)
            if text:
                out.append(text)
        return out

    def tool_call_arguments(self, names: list[str]) -> list[dict]:
        """Arguments of every assistant tool call whose tool is in `names`.

        A goal-mode turn delivers its result through `deliver_answer` /
        `update_goal` arguments rather than an assistant text block, so the
        delivery payload has to be read where the model actually wrote it.
        """
        out = []
        for row in dog.messages(self.home):
            if dog.payload_role(row) != "assistant":
                continue
            try:
                payload = json.loads(row.get("payload") or "{}")
            except Exception:
                continue
            for part in payload.get("content") or []:
                call = part.get("call") if isinstance(part, dict) else None
                if isinstance(call, dict) and call.get("name") in names:
                    out.append({"name": call.get("name"), "arguments": call.get("arguments")})
        return out

    def turn_statuses(self) -> list[str]:
        return [row.get("status") for row in dog.turns(self.home)]

    def wait_turn_terminal(self, after_count: int = 1, timeout: float = 600):
        def done():
            rows = dog.turns(self.home)
            if len(rows) < after_count:
                return None
            status = rows[-1].get("status")
            return rows[-1] if status not in ("running", None) else None

        return wait_until(done, timeout, 0.5)

    def wait_all_turns_terminal(self, waiting_since: float, timeout: float = 600):
        """Wait until no turn is running and one of them finished after `waiting_since`.

        `waiting_since` is an epoch second (float), not a string: a turn that
        finished in the same wall-clock second but BEFORE the wait started must
        not satisfy it. Second-truncated ISO strings made that mistake, so a
        just-cancelled turn could stand in for the new one that was still
        running.

        A continuation may resume the interrupted turn instead of opening a
        new ordinal, so waiting for a second row would encode an implementation
        detail the product does not promise.
        """

        def done():
            rows = dog.turns(self.home)
            if not rows or any(row.get("status") in ("running", None) for row in rows):
                return None
            finished = [
                (row, epoch_seconds(row.get("finished_at")))
                for row in rows
            ]
            recent = [
                row
                for row, stamp in finished
                if stamp is not None and stamp > waiting_since
            ]
            if not recent:
                return None
            return max(recent, key=lambda row: row.get("ordinal") or 0)

        return wait_until(done, timeout, 0.5)

    def frame_has(self, tui: Tui, needle: str, timeout: float) -> bool:
        return tui.wait_for(needle, timeout)

    # -- evidence ----------------------------------------------------------

    def finish(self):
        try:
            database_snapshot = dog.snapshot_database(self.home, self.dir)
            self.result.note("pre-cleanup SQLite checkpoint: " + (database_snapshot or "unavailable: no session database created"))
            snapshot = self.snapshot("final")
            if snapshot is not None:
                self.result.note("final status: %s" % snapshot.get("status"))
            self.result.note(
                "journal: %s"
                % json.dumps(
                    [
                        {k: row.get(k) for k in ("id", "content", "state", "ordinal")}
                        for row in dog.journal_rows(self.home)
                    ],
                    ensure_ascii=False,
                )
            )
            evidence = dog.database_evidence(self.home, [])
            with open(os.path.join(self.dir, "database.json"), "w", encoding="utf-8") as fh:
                json.dump(evidence, fh, ensure_ascii=False, indent=2)
            with open(os.path.join(self.dir, "session-inputs.json"), "w", encoding="utf-8") as fh:
                json.dump(evidence.get("session_inputs") or [], fh, ensure_ascii=False, indent=2)
            dog.snapshot_workspace(self.repo, self.dir)
            self.result.write(os.path.join(self.dir, "result.json"))
        finally:
            for tui in self.tuis:
                try:
                    tui.quit()
                except Exception:
                    pass
            self.stop_web()
            self.stop_daemon()
            for pid in self.replacement_runtimes:
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                except Exception:
                    pass
            for log in self.daemon_logs:
                log.close()
            if self.web_log:
                self.web_log.close()
            self.proxy.close()
            self.log.close()
            dog.finalize_evidence(self.dir)


def rewrite_model_limits(config_path: str, limits: dict):
    """Override the copied model entry's declared limits for a scenario."""
    text = open(config_path, encoding="utf-8").read()
    section = None
    out = []
    changed = 0
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            section = stripped
        if section == '[models."deepseek-v4.1-flash-1017"]':
            match = re.match(r"\s*(context_window|reliable_context|max_output_tokens)\s*=", line)
            if match and match.group(1) in limits:
                line = "%s = %d" % (match.group(1), limits[match.group(1)])
                changed += 1
        out.append(line)
    if changed != len(limits):
        raise SystemExit(
            "expected to rewrite %d model limits, rewrote %d" % (len(limits), changed)
        )
    with open(config_path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(out) + "\n")


# ---------------------------------------------------------------------------
# sc1: real daemon, two real TUIs, live steer, client reconnect
# ---------------------------------------------------------------------------


def sc1(args, repeat: int = 0) -> int:
    case = Case("sc1-dual-tui")
    result = case.result
    try:
        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        a = case.tui("A")
        a.spawn()
        sid = case.wait_session()
        result.check("session_created", sid is not None, str(sid))
        b = case.tui("B")
        b.spawn(sid)
        result.check(
            "second_client_attached",
            b.pty.alive() and sid in " ".join(b.argv),
            " ".join(b.argv),
        )

        start = time.time()
        a.type_and_enter(SLOW_PROMPT, settle=0.6)
        marker = os.path.join(case.repo, ".slow_started")
        result.check(
            "slow_command_started",
            bool(wait_until(lambda: os.path.exists(marker), 180)),
            marker,
        )
        running = case.snapshot("running-before-steer")
        result.check(
            "turn_running_in_daemon",
            bool(running and running.get("status") == "running"),
            str(running and running.get("status")),
        )

        steer_at = time.time()
        a.type_and_enter(STEER_COUNT, settle=0.4)
        admitted = wait_until(lambda: case.journal(STEER_COUNT), 30)
        result.check("journal_admitted", bool(admitted), json.dumps(admitted, ensure_ascii=False))
        result.check("journal_pending", bool(admitted and admitted[0]["state"] == "pending"), str(admitted and admitted[0]["state"]))
        result.check(
            "B_sees_admitted_row",
            case.frame_has(b, "循环一共会发出多少次 HTTP 请求", 30),
            "B screen",
        )
        time.sleep(1.0)
        a.dump("steer-admitted")
        b.dump("steer-admitted")
        ack_snapshot = case.snapshot("steer-admitted")
        steer_id = None
        if ack_snapshot:
            for message in ack_snapshot.get("messages") or []:
                if "循环一共会发出多少次 HTTP 请求" in (message.get("text") or ""):
                    steer_id = message.get("id")
        result.check(
            "same_runtime_message_id",
            bool(steer_id and admitted and steer_id == admitted[0]["id"]),
            "snapshot=%s journal=%s" % (steer_id, admitted and admitted[0]["id"]),
        )
        result.check(
            "B_row_state_matches_A",
            "已保存 · 等待投递" in a.text() and "已保存 · 等待投递" in b.text(),
            "both clients show 已保存 · 等待投递",
        )

        # A client process does not own the turn: killing B must not stop it,
        # and a fresh process must rebuild the same row from the same journal id.
        b.kill()
        b2 = case.tui("B2")
        b2.spawn(sid)
        result.check(
            "B_reconnected_sees_same_row",
            case.frame_has(b2, "循环一共会发出多少次 HTTP 请求", 30),
            "B2 screen",
        )
        b2.dump("reconnected")

        def consumed_row():
            rows = case.journal(STEER_COUNT)
            return rows[0] if rows and rows[0].get("state") == "consumed" else None

        consumed = wait_until(consumed_row, 240)
        result.check("journal_consumed", bool(consumed), str(case.journal(STEER_COUNT)))
        result.check(
            "A_sees_delivered_row",
            case.frame_has(a, "已进入当前任务", 60),
            "A screen",
        )
        result.check(
            "B2_sees_delivered_row",
            case.frame_has(b2, "已进入当前任务", 60),
            "B2 screen",
        )
        hit = case.wait_request_with(STEER_COUNT, after=steer_at, timeout=240)
        result.check("provider_received_steer", bool(hit), json.dumps(hit))
        occurrences = case.occurrences(STEER_COUNT, after=steer_at)
        result.check(
            "steer_once_per_request",
            all(row["count"] <= 1 for row in occurrences),
            json.dumps(occurrences),
        )
        rows = case.user_message_rows(STEER_COUNT)
        result.check("one_transcript_row", len(rows) == 1, str(len(rows)))
        result.check(
            "row_carries_journal_identity",
            bool(rows and rows[0].get("input_id") == case.journal(STEER_COUNT)[0]["id"]),
            str(rows and rows[0].get("input_id")),
        )

        terminal = case.wait_turn_terminal(1, timeout=420)
        result.check("turn_terminal", bool(terminal), str(terminal and terminal.get("status")))
        a.dump("turn-terminal")
        b2.dump("turn-terminal")
        final = case.assistant_texts()
        result.check(
            "final_answer_covers_steer",
            any("60" in text for text in final),
            (final[-1][-400:] if final else ""),
        )
        result.check(
            "no_duplicate_rows_on_screen",
            a.text().count("循环一共会发出多少次 HTTP 请求") <= 1
            and b2.text().count("循环一共会发出多少次 HTTP 请求") <= 1,
            "A=%d B2=%d"
            % (
                a.text().count("循环一共会发出多少次 HTTP 请求"),
                b2.text().count("循环一共会发出多少次 HTTP 请求"),
            ),
        )
    finally:
        case.finish()
    return 0 if all(check["ok"] for check in result.checks) else 1


# ---------------------------------------------------------------------------
# sc2: the daemon dies after the steer was acknowledged. The live TUI starts a
# replacement runtime, whose startup recovery settles the interrupted turn. The
# pending row must survive exactly once, the crash gate must be acknowledged,
# and the resumed turn must really execute the appended instruction.
# ---------------------------------------------------------------------------

CRASH_STEER = "追加：完成后在 README.md 末尾追加一行 PHASE3-CRASH-STEER。"


def sc2(args, repeat: int = 0) -> int:
    case = Case("sc2-daemon-crash")
    result = case.result
    try:
        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        a = case.tui("A")
        a.spawn()
        sid = case.wait_session()
        result.check("session_created", sid is not None, str(sid))

        a.type_and_enter(SLOW_PROMPT, settle=0.6)
        marker = os.path.join(case.repo, ".slow_started")
        result.check(
            "slow_command_started",
            bool(wait_until(lambda: os.path.exists(marker), 180)),
            marker,
        )
        steer_at = time.time()
        a.type_and_enter(CRASH_STEER, settle=0.4)
        admitted = wait_until(lambda: case.journal(CRASH_STEER), 30)
        result.check("journal_admitted", bool(admitted), json.dumps(admitted, ensure_ascii=False))
        result.check(
            "journal_pending_before_crash",
            bool(admitted and admitted[0]["state"] == "pending"),
            str(admitted and admitted[0]["state"]),
        )
        result.check(
            "frame_shows_saved",
            case.frame_has(a, "已保存 · 等待投递", 20),
            "A screen",
        )
        a.dump("before-crash")
        running = case.snapshot("before-crash")
        result.check(
            "turn_running_before_crash",
            bool(running and running.get("status") == "running"),
            str(running and running.get("status")),
        )

        case.kill_daemon()
        time.sleep(1.0)
        result.check(
            "daemon_dead",
            case.daemon is not None and case.daemon.poll() is not None,
            str(case.daemon and case.daemon.poll()),
        )
        a.dump("daemon-dead")

        # A live TUI does not stay orphaned: `ensure_default_runtime` starts a
        # new `leveler serve` child for the same repo. That process is the
        # recovered runtime, and its startup recovery settles the turn the
        # SIGKILL left running. Treat it as the real OS process it is.
        runtime = wait_until(lambda: case.replacement_runtime(a.pty.proc.pid), 60)
        result.check(
            "replacement_runtime_spawned",
            bool(runtime),
            json.dumps(runtime, ensure_ascii=False),
        )
        if runtime:
            result.note("replacement runtime: " + json.dumps(runtime, ensure_ascii=False))

        reconnected = wait_until(lambda: "命令运行中" not in a.text(), 90)
        a.dump("resynced")
        result.check(
            "stale_activity_cleared_after_resync",
            bool(reconnected),
            "A screen no longer claims the interrupted command runs",
        )
        interrupted = wait_until(
            lambda: next(
                (row for row in dog.turns(case.home) if row.get("status") == "interrupted"),
                None,
            ),
            60,
        )
        result.check("turn_marked_interrupted", bool(interrupted), str(interrupted))
        result.check(
            "pending_row_survived_crash",
            bool(case.journal(CRASH_STEER) and case.journal(CRASH_STEER)[0]["state"] == "pending"),
            json.dumps(case.journal(CRASH_STEER), ensure_ascii=False),
        )
        result.check(
            "frame_shows_pending_after_recovery",
            case.frame_has(a, "已保存 · 等待投递", 30),
            "A screen",
        )

        # The interrupted run_command gates new work: the text must be kept and
        # the client must ask for the explicit `--confirm-recovery` equivalent.
        resume_at = time.time()
        a.type_and_enter(
            "继续：请说明刚才发生了什么，并完成尚未完成的要求。不要只解释。", settle=0.5
        )
        gate = wait_until(lambda: "恢复确认" in a.text(), 60)
        a.dump("recovery-gate")
        result.check("recovery_gate_blocks_resume", bool(gate), "A screen")
        result.check(
            "resume_draft_preserved_by_gate",
            "并完成尚未完成的要求" in a.text(),
            "A screen still carries the typed draft",
        )

        # The evidence rows are selectable only as evidence; the cursor starts
        # on cancel, so one step down is the acknowledgement.
        a.key("\x1b[B", "\r")
        acknowledged = wait_until(lambda: "已确认恢复" in a.text(), 30)
        a.dump("recovery-acknowledged")
        result.check("recovery_acknowledged", bool(acknowledged), "A screen")
        result.check(
            "recovery_overlay_closed",
            bool(wait_until(lambda: "恢复确认" not in a.text(), 30)),
            "A screen",
        )

        # Acknowledging releases the gate; it does not send the draft.
        a.key("\r")
        hit = case.wait_request_with(CRASH_STEER, after=resume_at, timeout=300)
        result.check("provider_received_after_resume", bool(hit), json.dumps(hit))
        occurrences = case.occurrences(CRASH_STEER, after=resume_at)
        result.check(
            "steer_once_per_request_after_resume",
            all(row["count"] <= 1 for row in occurrences),
            json.dumps(occurrences),
        )
        consumed = wait_until(
            lambda: (case.journal(CRASH_STEER) or [{}])[0].get("state") == "consumed", 300
        )
        result.check("journal_consumed_after_resume", bool(consumed), str(case.journal(CRASH_STEER)))
        rows = case.user_message_rows(CRASH_STEER)
        result.check("one_transcript_row_after_resume", len(rows) == 1, str(len(rows)))
        result.check(
            "row_carries_journal_identity",
            bool(
                rows
                and case.journal(CRASH_STEER)
                and rows[0].get("input_id") == case.journal(CRASH_STEER)[0]["id"]
            ),
            str(rows and rows[0].get("input_id")),
        )

        terminal = case.wait_all_turns_terminal(resume_at, timeout=420)
        result.check("resumed_turn_terminal", bool(terminal), str(terminal and terminal.get("status")))
        a.dump("resumed-terminal")
        readme = os.path.join(case.repo, "README.md")
        content = open(readme, encoding="utf-8").read() if os.path.exists(readme) else ""
        result.check(
            "steer_executed_in_workspace",
            "PHASE3-CRASH-STEER" in content,
            "README.md contains the appended line",
        )
        final = case.assistant_texts()
        result.check(
            "final_answer_after_resume",
            bool(final),
            (final[-1][-300:] if final else ""),
        )
    finally:
        case.finish()
    return 0 if all(check["ok"] for check in result.checks) else 1


# ---------------------------------------------------------------------------
# sc3: compaction. A steer admitted before the fold must be inside the summary
# request; a steer admitted while the fold is in flight must survive it and be
# delivered afterwards. Both must be executed, not merely journaled.
# ---------------------------------------------------------------------------

COMPACT_PROMPT = (
    "请用 run_command 运行 python3，args 为 "
    '["-c", "import time; time.sleep(12); print(\'X\'*46000)"]'
    "（timeout_seconds=60，不要开启 background）。等命令结束后，只告诉我 stdout 的字符数。"
    "不要修改任何文件。"
)
COMPACT_BEFORE = "追加：命令结束后，请在 README.md 末尾追加一行 PHASE3-COMPACT-BEFORE。"
COMPACT_DURING = "追加：另外请在 README.md 末尾追加一行 PHASE3-COMPACT-DURING。"


def sc3(args, repeat: int = 0) -> int:
    case = Case(
        "sc3-compaction",
        model_limits={
            # The real fold condition is `projection > min(95% × capacity,
            # reliable_context)`. This turn's inline result is ~24.8 KB
            # (~9.9k tokens at tool density), so the projection at the round
            # top is ~20k tokens. 24k put the threshold just above it and the
            # real runtime correctly never folded. 16k keeps a wide margin
            # while still leaving the fixed prefix (tools + system + control)
            # below it, so the fold fires exactly once on the large round.
            "context_window": 40000,
            "reliable_context": 16000,
            "max_output_tokens": 2048,
        },
    )
    case.proxy.compaction_delay = 10.0
    result = case.result
    try:
        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        a = case.tui("A")
        a.spawn()
        sid = case.wait_session()
        result.check("session_created", sid is not None, str(sid))

        start_iso = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
        start_at = time.time()
        a.type_and_enter(COMPACT_PROMPT, settle=0.6)

        def slow_tool():
            return case.tool_started("run_command", start_iso) or case.tool_started(
                "shell_command", start_iso
            )

        started = wait_until(slow_tool, 180)
        result.check("large_output_command_started", bool(started), str(started and started[0].get("created_at")))

        # The command sleeps 12s, so this admission is pending at the round top
        # that folds the transcript.
        a.type_and_enter(COMPACT_BEFORE, settle=0.4)
        before_row = wait_until(lambda: case.journal(COMPACT_BEFORE), 30)
        result.check("before_steer_admitted", bool(before_row), json.dumps(before_row, ensure_ascii=False))

        spans = wait_until(lambda: case.compaction_spans(), 300)
        result.check("compaction_happened", bool(spans), json.dumps(spans))
        span = spans[0] if spans else None

        # The fold is sleeping in the proxy: admit the second steer while the
        # summary request is already in flight.
        during_at = time.time()
        a.type_and_enter(COMPACT_DURING, settle=0.4)
        during_row = wait_until(lambda: case.journal(COMPACT_DURING), 30)
        result.check("during_steer_admitted", bool(during_row), json.dumps(during_row, ensure_ascii=False))

        if before_row and span:
            admitted = dog.iso_epoch(before_row[0].get("created_at"))
            result.check(
                "before_steer_precedes_fold",
                admitted is not None and span.get("start") is not None and admitted <= span["start"],
                "admitted=%s fold=%s" % (admitted, span.get("start")),
            )
        if during_row and span:
            admitted = dog.iso_epoch(during_row[0].get("created_at"))
            result.check(
                "during_steer_arrives_mid_fold",
                admitted is not None
                and span.get("start") is not None
                and admitted >= span["start"],
                "admitted=%s fold_start=%s fold_end=%s"
                % (admitted, span.get("start"), span.get("end")),
            )

        summaries = wait_until(lambda: case.compaction_requests(), 120)
        result.check("compaction_request_observed", bool(summaries), str(len(summaries or [])))
        if summaries:
            summary_seq = summaries[0][0]["seq"]
            summary_body = summaries[0][1]
            summary_text = "\n".join(
                body_text(message) for message in (summary_body.get("messages") or [])
            )
            # A message consumed just before the fold is normally kept in the
            # retention tail rather than duplicated into the summary. What
            # matters is that it reached the model before the fold and is
            # still in the model context after it.
            before_in_summary = COMPACT_BEFORE in summary_text
            before_pre_fold = any(
                record.get("seq") is not None
                and record["seq"] < summary_seq
                and COMPACT_BEFORE
                in json.dumps(body.get("messages") or [], ensure_ascii=False)
                for record, body in case.request_bodies()
            )
            before_post_fold = wait_until(
                lambda: any(
                    record.get("seq") is not None
                    and record["seq"] > summary_seq
                    and COMPACT_BEFORE
                    in json.dumps(body.get("messages") or [], ensure_ascii=False)
                    for record, body in case.request_bodies()
                ),
                120,
            )
            result.check(
                "before_steer_reached_the_model_before_the_fold",
                before_pre_fold,
                "compaction seq %s" % summary_seq,
            )
            result.check(
                "before_steer_survives_the_fold",
                before_in_summary or before_post_fold,
                "in_summary=%s post_fold=%s" % (before_in_summary, before_post_fold),
            )
            result.check(
                "during_steer_is_not_in_the_summary",
                COMPACT_DURING not in summary_text,
                "compaction seq %s" % summary_seq,
            )
            later = case.wait_request_with(COMPACT_DURING, after=during_at, timeout=300)
            result.check("during_steer_reaches_a_later_request", bool(later), json.dumps(later))
            result.check(
                "execution_resumes_after_the_fold",
                any(record["seq"] > summary_seq for record, _body in case.request_bodies()),
                "requests after compaction seq %s" % summary_seq,
            )
        else:
            result.check("during_steer_reaches_a_later_request", False, "no compaction request")
            result.check("during_steer_is_not_in_the_summary", False, "no compaction request")
            result.check("execution_resumes_after_the_fold", False, "no compaction request")
            result.check(
                "before_steer_reached_the_model_before_the_fold", False, "no compaction request"
            )
            result.check("before_steer_survives_the_fold", False, "no compaction request")

        for label, text in (("before", COMPACT_BEFORE), ("during", COMPACT_DURING)):
            consumed = wait_until(
                lambda t=text: (case.journal(t) or [{}])[0].get("state") == "consumed", 420
            )
            result.check("journal_consumed_%s_steer" % label, bool(consumed), str(case.journal(text)))
            rows = case.user_message_rows(text)
            result.check("one_transcript_row_%s_steer" % label, len(rows) == 1, str(len(rows)))
            result.check(
                "row_carries_journal_identity_%s_steer" % label,
                bool(
                    rows
                    and case.journal(text)
                    and rows[0].get("input_id") == case.journal(text)[0]["id"]
                ),
                str(rows and rows[0].get("input_id")),
            )

        terminal = case.wait_all_turns_terminal(start_at, timeout=600)
        result.check("turn_terminal", bool(terminal), str(terminal and terminal.get("status")))
        a.dump("terminal")
        readme = os.path.join(case.repo, "README.md")
        content = open(readme, encoding="utf-8").read() if os.path.exists(readme) else ""
        result.check("before_steer_executed", "PHASE3-COMPACT-BEFORE" in content, "README")
        result.check("during_steer_executed", "PHASE3-COMPACT-DURING" in content, "README")
        kinds = case.model_request_kinds()
        result.check(
            "model_requests_record_the_compaction",
            any(row.get("kind") == "compaction" for row in kinds),
            json.dumps([row.get("kind") for row in kinds]),
        )
        final = case.assistant_texts()
        result.check("final_answer", bool(final), (final[-1][-300:] if final else ""))
    finally:
        case.finish()
    return 0 if all(check["ok"] for check in result.checks) else 1


# ---------------------------------------------------------------------------
# sc4: a steer while wait_task holds the round. The wait must yield at the
# user's message, the background process must keep running, and the steer must
# be executed in the same turn.
# ---------------------------------------------------------------------------

WAIT_PROMPT = (
    "请用 run_command 在后台启动 python3，args 为 "
    '["-c", "import time; time.sleep(45); print(\'PHASE3-WAIT-READY\')"]'
    "（background=true，background_lifetime=\"runtime\"）。拿到 task_id 后立刻用 wait_task 等它结束"
    "（timeout_seconds=600），不要用其他方式轮询。它结束后把 stdout 的最后一行报告给我。"
)
WAIT_STEER = (
    "追加：请继续等它结束；结束后在 README.md 末尾追加一行 PHASE3-WAIT-STEER，"
    "并在最终回答里给出后台进程打印的最后一行。"
)


def sc4(args, repeat: int = 0) -> int:
    case = Case("sc4-wait-task")
    result = case.result
    try:
        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        a = case.tui("A")
        a.spawn()
        sid = case.wait_session()
        result.check("session_created", sid is not None, str(sid))

        start_iso = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
        a.type_and_enter(WAIT_PROMPT, settle=0.6)
        waiting = wait_until(lambda: case.tool_started("wait_task", start_iso), 240)
        result.check("wait_task_started", bool(waiting), str(waiting and waiting[0].get("created_at")))
        background = case.tool_started("run_command", start_iso)
        result.check(
            "background_process_started_once",
            len(background) == 1 and "\\\"background\\\":true" in (background[0].get("payload") or ""),
            json.dumps([row.get("payload", "")[:160] for row in background], ensure_ascii=False),
        )
        command_started = dog.iso_epoch(background[0].get("created_at")) if background else None

        steer_at = time.time()
        # Capture the optimistic row immediately: wait_task yields as soon as
        # the user message lands, so the pending label can be visible for less
        # than a second. Querying the journal first loses that frame.
        a.type_and_enter(WAIT_STEER, settle=0.2)
        saved_seen = case.frame_has(a, "已保存 · 等待投递", 2.0)
        admitted = wait_until(lambda: case.journal(WAIT_STEER), 30)
        result.check("journal_admitted", bool(admitted), json.dumps(admitted, ensure_ascii=False))
        result.check(
            "frame_shows_saved",
            saved_seen or case.frame_has(a, "已进入当前任务", 5.0),
            "A screen (saved=%s)" % saved_seen,
        )

        hit = case.wait_request_with(WAIT_STEER, after=steer_at, timeout=120)
        result.check("steer_reached_provider", bool(hit), json.dumps(hit))
        if hit and command_started:
            result.check(
                "wait_yielded_before_the_process_exited",
                hit["t"] - command_started < 40,
                "steer request %.1fs after the 45s process started"
                % (hit["t"] - command_started),
            )
        occurrences = case.occurrences(WAIT_STEER, after=steer_at)
        result.check(
            "steer_once_per_request",
            all(row["count"] <= 1 for row in occurrences),
            json.dumps(occurrences),
        )
        consumed = wait_until(
            lambda: (case.journal(WAIT_STEER) or [{}])[0].get("state") == "consumed", 300
        )
        result.check("journal_consumed", bool(consumed), str(case.journal(WAIT_STEER)))
        rows = case.user_message_rows(WAIT_STEER)
        result.check("one_transcript_row", len(rows) == 1, str(len(rows)))
        result.check(
            "row_carries_journal_identity",
            bool(rows and case.journal(WAIT_STEER) and rows[0].get("input_id") == case.journal(WAIT_STEER)[0]["id"]),
            str(rows and rows[0].get("input_id")),
        )

        terminal = case.wait_all_turns_terminal(steer_at, timeout=600)
        result.check("turn_terminal", bool(terminal), str(terminal and terminal.get("status")))
        a.dump("terminal")
        readme = os.path.join(case.repo, "README.md")
        content = open(readme, encoding="utf-8").read() if os.path.exists(readme) else ""
        result.check("steer_executed", "PHASE3-WAIT-STEER" in content, "README")
        starts = case.tool_started("run_command", start_iso)
        result.check(
            "background_process_not_restarted",
            len(starts) == 1,
            "%d run_command starts" % len(starts),
        )
        final = case.assistant_texts()
        result.check(
            "background_result_reported",
            any("PHASE3-WAIT-READY" in text for text in final),
            (final[-1][-300:] if final else ""),
        )
        result.check("final_answer", bool(final), (final[-1][-300:] if final else ""))
    finally:
        case.finish()
    return 0 if all(check["ok"] for check in result.checks) else 1


# ---------------------------------------------------------------------------
# sc5: a steer inside a `/goal` window. The journal and the durable goal
# amendment are both written, the model is asked again, and the goal work
# carries the new requirement to completion.
# ---------------------------------------------------------------------------

GOAL_PROMPT = (
    "/goal 请运行 python3 -m unittest tests.test_00_slow -v（timeout_seconds=120），"
    "确认全部通过，并把结果写入 README.md 的 PHASE3-GOAL 一节。"
)
GOAL_STEER = (
    "追加：另外请在 README.md 末尾追加一行 PHASE3-GOAL-STEER，"
    "并在最终回答里写明 tests/test_00_slow.py 的循环一共会发出多少次 HTTP 请求。"
)


def sc5(args, repeat: int = 0) -> int:
    case = Case("sc5-goal")
    result = case.result
    try:
        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        a = case.tui("A")
        a.spawn()
        sid = case.wait_session()
        result.check("session_created", sid is not None, str(sid))

        a.type_and_enter(GOAL_PROMPT, settle=0.6)
        marker = os.path.join(case.repo, ".slow_started")
        result.check(
            "goal_command_started",
            bool(wait_until(lambda: os.path.exists(marker), 180)),
            marker,
        )
        goal_rows = dog.query(case.home, "SELECT * FROM goals")
        result.check("goal_opened", len(goal_rows) == 1, json.dumps(goal_rows, ensure_ascii=False))

        steer_at = time.time()
        a.type_and_enter(GOAL_STEER, settle=0.2)
        saved_seen = case.frame_has(a, "已保存 · 等待投递", 2.0)
        admitted = wait_until(lambda: case.journal(GOAL_STEER), 30)
        result.check("journal_admitted", bool(admitted), json.dumps(admitted, ensure_ascii=False))
        amended = wait_until(
            lambda: any(
                GOAL_STEER in (row.get("amendments") or "")
                for row in dog.query(case.home, "SELECT amendments FROM goals")
            ),
            60,
        )
        result.check("goal_amendment_recorded", bool(amended), "goals.amendments")
        result.check(
            "frame_shows_saved",
            saved_seen or case.frame_has(a, "已进入当前任务", 5.0),
            "A screen (saved=%s)" % saved_seen,
        )
        hit = case.wait_request_with(GOAL_STEER, after=steer_at, timeout=240)
        result.check("steer_reached_provider", bool(hit), json.dumps(hit))
        occurrences = case.occurrences(GOAL_STEER, after=steer_at)
        result.check(
            "steer_once_per_request",
            all(row["count"] <= 1 for row in occurrences),
            json.dumps(occurrences),
        )
        consumed = wait_until(
            lambda: (case.journal(GOAL_STEER) or [{}])[0].get("state") == "consumed", 420
        )
        result.check("journal_consumed", bool(consumed), str(case.journal(GOAL_STEER)))
        rows = case.user_message_rows(GOAL_STEER)
        result.check("one_transcript_row", len(rows) == 1, str(len(rows)))
        result.check(
            "row_carries_journal_identity",
            bool(rows and case.journal(GOAL_STEER) and rows[0].get("input_id") == case.journal(GOAL_STEER)[0]["id"]),
            str(rows and rows[0].get("input_id")),
        )

        terminal = case.wait_all_turns_terminal(steer_at, timeout=600)
        result.check("turn_terminal", bool(terminal), str(terminal and terminal.get("status")))
        a.dump("terminal")
        readme = os.path.join(case.repo, "README.md")
        content = open(readme, encoding="utf-8").read() if os.path.exists(readme) else ""
        result.check("goal_section_written", "PHASE3-GOAL" in content, "README")
        result.check("steer_executed", "PHASE3-GOAL-STEER" in content, "README")
        final = case.assistant_texts()
        result.check(
            "final_answer_covers_the_steer",
            any("60" in text for text in final),
            (final[-1][-300:] if final else ""),
        )
        goal_rows = dog.query(case.home, "SELECT state, amendments FROM goals")
        result.check("goal_state_recorded", bool(goal_rows), json.dumps(goal_rows, ensure_ascii=False))
    finally:
        case.finish()
    return 0 if all(check["ok"] for check in result.checks) else 1


# ---------------------------------------------------------------------------
# sc6: a non-cancel turn failure with an unconsumed steer. The provider refuses
# the turn before any round consumes the journal row; the row must survive and
# the runtime must promote it exactly once into the next turn.
# ---------------------------------------------------------------------------

FAIL_PROMPT = (
    "请运行 python3 -m unittest tests.test_00_slow -v（timeout_seconds=120），然后汇报结果。"
)
FAIL_STEER = "追加：在 README.md 末尾追加一行 PHASE3-FAIL-STEER，并告诉我当前工作目录。"


def sc6(args, repeat: int = 0) -> int:
    case = Case("sc6-failed-turn")
    result = case.result
    try:
        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        a = case.tui("A")
        a.spawn()
        sid = case.wait_session()
        result.check("session_created", sid is not None, str(sid))

        # A 400 is a non-retryable provider refusal, held open long enough for
        # the user's message to be admitted before the turn dies.
        case.proxy.fail_next(1, delay=8.0, status=400)
        a.type_and_enter(FAIL_PROMPT, settle=0.6)

        def faulted():
            return next(
                (
                    record
                    for record in case.proxy_records()
                    if record.get("kind") == "request_start"
                    and record.get("fault") == "http_400"
                ),
                None,
            )

        fault = wait_until(faulted, 60)
        result.check("provider_fault_injected", bool(fault), json.dumps(fault, ensure_ascii=False))
        steer_at = time.time()
        a.type_and_enter(FAIL_STEER, settle=0.4)
        admitted = wait_until(lambda: case.journal(FAIL_STEER), 30)
        result.check("journal_admitted", bool(admitted), json.dumps(admitted, ensure_ascii=False))
        result.check(
            "journal_pending_during_turn",
            bool(admitted and admitted[0]["state"] == "pending"),
            str(admitted and admitted[0]["state"]),
        )

        failed = wait_until(
            lambda: next(
                (row for row in dog.turns(case.home) if row.get("status") == "failed"),
                None,
            ),
            60,
        )
        result.check("turn_failed_without_cancel", bool(failed), str(failed and failed.get("status")))
        result.check(
            "journal_not_cancelled_by_failure",
            bool(case.journal(FAIL_STEER) and case.journal(FAIL_STEER)[0]["state"] != "cancelled"),
            json.dumps(case.journal(FAIL_STEER), ensure_ascii=False),
        )

        promoted = wait_until(
            lambda: (case.journal(FAIL_STEER) or [{}])[0].get("state") == "consumed", 300
        )
        result.check("pending_steer_promoted_after_failure", bool(promoted), str(case.journal(FAIL_STEER)))
        hit = case.wait_request_with(FAIL_STEER, after=steer_at, timeout=300)
        result.check("steer_reached_provider_after_failure", bool(hit), json.dumps(hit))
        occurrences = case.occurrences_outside_objective_pin(FAIL_STEER, after=steer_at)
        result.check(
            "steer_once_per_request",
            all(row["count"] <= 1 for row in occurrences),
            json.dumps(occurrences),
        )
        rows = case.user_message_rows(FAIL_STEER)
        result.check("one_transcript_row", len(rows) == 1, str(len(rows)))
        result.check(
            "row_carries_journal_identity",
            bool(rows and case.journal(FAIL_STEER) and rows[0].get("input_id") == case.journal(FAIL_STEER)[0]["id"]),
            str(rows and rows[0].get("input_id")),
        )
        result.check(
            "exactly_two_turns",
            len(dog.turns(case.home)) == 2,
            str([row.get("status") for row in dog.turns(case.home)]),
        )

        promoted_terminal = case.wait_all_turns_terminal(steer_at, timeout=600)
        result.check(
            "promoted_turn_terminal",
            bool(promoted_terminal),
            str(promoted_terminal and promoted_terminal.get("status")),
        )
        a.dump("terminal")
        readme = os.path.join(case.repo, "README.md")
        content = open(readme, encoding="utf-8").read() if os.path.exists(readme) else ""
        result.check("steer_executed_after_failure", "PHASE3-FAIL-STEER" in content, "README")
        final = case.assistant_texts()
        result.check("final_answer", bool(final), (final[-1][-300:] if final else ""))
    finally:
        case.finish()
    return 0 if all(check["ok"] for check in result.checks) else 1


# ---------------------------------------------------------------------------
# sc7: several appends in a row, then a crash. Every journal row must survive
# the SIGKILL as the same durable record, be consumed exactly once after the
# acknowledged recovery, and be executed.
# ---------------------------------------------------------------------------

MULTI_STEERS = [
    "追加：命令结束后，请在 README.md 末尾追加一行 PHASE3-MULTI-ONE。",
    "追加：另外请在 README.md 末尾追加一行 PHASE3-MULTI-TWO。",
    "追加：另外请在 README.md 末尾追加一行 PHASE3-MULTI-THREE。",
]
MULTI_MARKERS = ["PHASE3-MULTI-ONE", "PHASE3-MULTI-TWO", "PHASE3-MULTI-THREE"]


def sc7(args, repeat: int = 0) -> int:
    case = Case("sc7-multi-append-crash")
    result = case.result
    try:
        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        a = case.tui("A")
        a.spawn()
        sid = case.wait_session()
        result.check("session_created", sid is not None, str(sid))

        a.type_and_enter(SLOW_PROMPT, settle=0.6)
        marker = os.path.join(case.repo, ".slow_started")
        result.check(
            "slow_command_started",
            bool(wait_until(lambda: os.path.exists(marker), 180)),
            marker,
        )
        for index, text in enumerate(MULTI_STEERS):
            a.type_and_enter(text, settle=0.35)
            wait_until(lambda count=index + 1: len(case.journal_all()) >= count, 20)

        admitted = wait_until(
            lambda: len(case.journal_all()) >= len(MULTI_STEERS) and case.journal_all(), 30
        )
        result.check(
            "all_appends_admitted",
            bool(admitted and len(admitted) == len(MULTI_STEERS)),
            json.dumps(admitted, ensure_ascii=False),
        )
        result.check(
            "all_appends_pending_before_crash",
            bool(admitted and all(row.get("state") == "pending" for row in admitted)),
            json.dumps([row.get("state") for row in (admitted or [])]),
        )
        result.check(
            "append_order_preserved",
            bool(admitted and [row.get("content") for row in admitted] == MULTI_STEERS),
            json.dumps([row.get("content") for row in (admitted or [])], ensure_ascii=False),
        )
        admitted_ids = [row.get("id") for row in (admitted or [])]
        result.check(
            "frame_shows_saved",
            case.frame_has(a, "已保存 · 等待投递", 20),
            "A screen",
        )
        a.dump("before-crash")
        result.check(
            "turn_running_before_crash",
            (case.snapshot("before-crash") or {}).get("status") == "running",
            "daemon snapshot",
        )

        case.kill_daemon()
        time.sleep(1.0)
        result.check(
            "daemon_dead",
            case.daemon is not None and case.daemon.poll() is not None,
            str(case.daemon and case.daemon.poll()),
        )
        runtime = wait_until(lambda: case.replacement_runtime(a.pty.proc.pid), 60)
        result.check(
            "replacement_runtime_spawned",
            bool(runtime),
            json.dumps(runtime, ensure_ascii=False),
        )
        resynced = wait_until(lambda: "命令运行中" not in a.text(), 90)
        a.dump("resynced")
        result.check("stale_activity_cleared_after_resync", bool(resynced), "A screen")
        interrupted = wait_until(
            lambda: next(
                (row for row in dog.turns(case.home) if row.get("status") == "interrupted"),
                None,
            ),
            60,
        )
        result.check("turn_marked_interrupted", bool(interrupted), str(interrupted))

        survived = case.journal_all()
        result.check(
            "all_appends_survived_crash",
            len(survived) == len(MULTI_STEERS)
            and all(row.get("state") == "pending" for row in survived),
            json.dumps(
                [{"id": row.get("id"), "state": row.get("state")} for row in survived],
                ensure_ascii=False,
            ),
        )
        result.check(
            "journal_rows_are_the_same_records",
            [row.get("id") for row in survived] == admitted_ids,
            "before=%s after=%s" % (admitted_ids, [row.get("id") for row in survived]),
        )
        result.check(
            "frame_shows_pending_after_recovery",
            case.frame_has(a, "已保存 · 等待投递", 30),
            "A screen",
        )

        resume_at = time.time()
        a.type_and_enter("继续：完成原任务和全部追加要求，不要只解释。", settle=0.5)
        gate = wait_until(lambda: "恢复确认" in a.text(), 60)
        a.dump("recovery-gate")
        result.check("recovery_gate_blocks_resume", bool(gate), "A screen")
        a.key("\x1b[B", "\r")
        result.check(
            "recovery_acknowledged",
            bool(wait_until(lambda: "已确认恢复" in a.text(), 30)),
            "A screen",
        )
        result.check(
            "recovery_overlay_closed",
            bool(wait_until(lambda: "恢复确认" not in a.text(), 30)),
            "A screen",
        )
        a.key("\r")

        for index, text in enumerate(MULTI_STEERS):
            hit = case.wait_request_with(text, after=resume_at, timeout=240)
            result.check(
                "steer_%d_reached_provider" % (index + 1),
                bool(hit),
                json.dumps(hit),
            )
            occurrences = case.occurrences(text, after=resume_at)
            result.check(
                "steer_%d_once_per_request" % (index + 1),
                all(row["count"] <= 1 for row in occurrences),
                json.dumps(occurrences),
            )

        consumed = wait_until(
            lambda: all(row.get("state") == "consumed" for row in case.journal_all()), 420
        )
        result.check(
            "all_appends_consumed",
            bool(consumed),
            json.dumps([row.get("state") for row in case.journal_all()]),
        )
        identities = []
        for index, text in enumerate(MULTI_STEERS):
            rows = case.user_message_rows(text)
            result.check(
                "one_transcript_row_%d" % (index + 1),
                len(rows) == 1,
                str(len(rows)),
            )
            journal = case.journal(text)
            result.check(
                "row_carries_journal_identity_%d" % (index + 1),
                bool(rows and journal and rows[0].get("input_id") == journal[0]["id"]),
                str(rows and rows[0].get("input_id")),
            )
            identities.extend(row.get("id") for row in rows)
        result.check(
            "transcript_rows_are_distinct",
            len(identities) == len(set(identities)) == len(MULTI_STEERS),
            json.dumps(identities),
        )

        terminal = case.wait_all_turns_terminal(resume_at, timeout=600)
        result.check("resumed_turn_terminal", bool(terminal), str(terminal and terminal.get("status")))
        a.dump("resumed-terminal")
        readme = os.path.join(case.repo, "README.md")
        content = open(readme, encoding="utf-8").read() if os.path.exists(readme) else ""
        for index, marker_text in enumerate(MULTI_MARKERS):
            result.check(
                "steer_%d_executed_once" % (index + 1),
                content.count(marker_text) == 1,
                "README occurrences=%d" % content.count(marker_text),
            )
        final = case.assistant_texts()
        result.check("final_answer_after_resume", bool(final), (final[-1][-300:] if final else ""))
    finally:
        case.finish()
    return 0 if all(check["ok"] for check in result.checks) else 1


# ---------------------------------------------------------------------------
# sc8: a cancelled turn. The appended instruction is accepted, then settled as
# cancelled with the task. A later unrelated turn must neither receive it nor
# resurrect it; the cancelled record stays visible exactly once.
# ---------------------------------------------------------------------------

CANCEL_STEER = "追加：在 README.md 末尾追加一行 PHASE3-CANCEL-STEER。"
CANCEL_LATER = "只回答一个问题：当前目录下的 README.md 是否存在？不要修改任何文件，也不要运行测试。"


def sc8(args, repeat: int = 0) -> int:
    case = Case("sc8-cancelled-append")
    result = case.result
    try:
        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        a = case.tui("A")
        a.spawn()
        sid = case.wait_session()
        result.check("session_created", sid is not None, str(sid))

        a.type_and_enter(SLOW_PROMPT, settle=0.6)
        marker = os.path.join(case.repo, ".slow_started")
        result.check(
            "slow_command_started",
            bool(wait_until(lambda: os.path.exists(marker), 180)),
            marker,
        )
        steer_at = time.time()
        a.type_and_enter(CANCEL_STEER, settle=0.4)
        admitted = wait_until(lambda: case.journal(CANCEL_STEER), 30)
        result.check("journal_admitted", bool(admitted), json.dumps(admitted, ensure_ascii=False))
        result.check(
            "journal_pending_before_cancel",
            bool(admitted and admitted[0]["state"] == "pending"),
            str(admitted and admitted[0]["state"]),
        )
        result.check(
            "frame_shows_saved",
            case.frame_has(a, "已保存 · 等待投递", 20),
            "A screen",
        )

        a.key("\x1b")
        # The durable turn status for a user cancel during an active tool call
        # is `interrupted` (the command's outcome is unknown); the cancel
        # itself is recorded as `stop_reason: "cancelled"` on turn_finished.
        # Both are the product's own facts — a completed turn would not be.
        def cancelled_stop():
            for row in dog.query(
                case.home,
                "SELECT payload FROM events WHERE type = 'turn_finished' ORDER BY sequence",
            ):
                try:
                    payload = json.loads(row.get("payload") or "{}")
                except Exception:
                    continue
                stop = payload.get("payload") or payload
                if stop.get("stop_reason") == "cancelled":
                    return stop
            return None

        cancelled_turn = wait_until(cancelled_stop, 120)
        result.check("turn_cancelled", bool(cancelled_turn), json.dumps(cancelled_turn))
        cancel_row = wait_until(
            lambda: (case.journal(CANCEL_STEER) or [{}])[0].get("state") == "cancelled", 60
        )
        result.check("journal_cancelled", bool(cancel_row), str(case.journal(CANCEL_STEER)))
        result.check(
            "frame_shows_cancelled_input",
            case.frame_has(a, "未发送 · 任务已取消", 30),
            "A screen",
        )
        a.dump("cancelled")

        after_cancel = time.time()
        a.type_and_enter(CANCEL_LATER, settle=0.5)
        terminal = case.wait_all_turns_terminal(after_cancel, timeout=300)
        result.check("later_turn_terminal", bool(terminal), str(terminal and terminal.get("status")))
        a.dump("later-terminal")

        leaked = case.occurrences(CANCEL_STEER, after=after_cancel)
        result.check(
            "cancelled_input_never_reaches_the_provider",
            all(row["count"] == 0 for row in leaked),
            json.dumps(leaked),
        )
        rows = case.user_message_rows(CANCEL_STEER)
        result.check("no_transcript_row_for_cancelled_input", len(rows) == 0, str(len(rows)))
        result.check(
            "journal_stays_cancelled",
            bool(case.journal(CANCEL_STEER) and case.journal(CANCEL_STEER)[0]["state"] == "cancelled"),
            str(case.journal(CANCEL_STEER)),
        )
        readme = os.path.join(case.repo, "README.md")
        content = open(readme, encoding="utf-8").read() if os.path.exists(readme) else ""
        result.check("cancelled_input_not_executed", "PHASE3-CANCEL-STEER" not in content, "README")
        snapshot = case.snapshot("after-later-turn") or {}
        visible = [
            message
            for message in (snapshot.get("messages") or [])
            if CANCEL_STEER in (message.get("text") or "")
        ]
        result.check(
            "cancelled_input_stays_visible_once",
            len(visible) == 1 and visible[0].get("kind") == "cancelled_input",
            json.dumps(visible, ensure_ascii=False),
        )
    finally:
        case.finish()
    return 0 if all(check["ok"] for check in result.checks) else 1


# ---------------------------------------------------------------------------
# product: the real scenario from the product closure. The user asks for a
# layout improvement; while the baseline suite runs they add a missing feature
# in the same breath. Delivery alone proves nothing: the page must actually be
# implemented, the existing contract must stay green, and the final answer must
# account for the addition the user actually made.
# ---------------------------------------------------------------------------

ACCESS_FIXTURE = os.path.join(ROOT, "testdata", "append-instruction-dogfood", "access-portal")
ACCESS_PROMPT = (
    "优化 Access 页面布局。请先运行项目现有测试了解基线："
    "python3 -m unittest discover -s tests -v（timeout_seconds=180）。"
    "然后按 tests/test_access_layout.py 的契约完成布局改造，并实际请求页面确认。"
    "不要只回复说明，要实际修改和验证。"
)
ACCESS_STEER = "还有管理员管理也没有。"

ACCESS_PROBE_SRC = r'''
import json, re, threading, urllib.request
from app import make_server

server = make_server(port=0)
threading.Thread(target=server.serve_forever, daemon=True).start()
base = "http://127.0.0.1:%d" % server.server_address[1]

def get(path):
    try:
        with urllib.request.urlopen(base + path, timeout=5) as response:
            return {
                "status": response.status,
                "type": response.headers.get("Content-Type", ""),
                "body": response.read().decode("utf-8", "replace"),
            }
    except Exception as exc:
        body = ""
        try:
            body = exc.read().decode("utf-8", "replace")
        except Exception:
            pass
        return {"status": getattr(exc, "code", None), "type": "", "body": body}

pages = {path: get(path) for path in ("/access", "/roles", "/admin", "/static/app.css")}
links = sorted(set(re.findall(r'href="(/[^"#?]*)"', pages["/access"]["body"])))
for link in links:
    if link not in pages:
        pages[link] = get(link)
server.shutdown()
server.server_close()
print(json.dumps({"pages": pages, "links": links}, ensure_ascii=False))
'''


def run_python_tests(repo: str, target: list[str], timeout: float = 300) -> dict:
    argv = [sys.executable, "-m", "unittest"] + target + ["-v"]
    try:
        proc = subprocess.run(argv, cwd=repo, capture_output=True, text=True, timeout=timeout)
        return {
            "argv": argv,
            "code": proc.returncode,
            "out": proc.stdout[-4000:],
            "err": proc.stderr[-2000:],
        }
    except subprocess.TimeoutExpired as exc:
        return {"argv": argv, "code": "timeout", "out": "", "err": "timed out"}


def probe_access_pages(case: Case, label: str) -> dict | None:
    try:
        proc = subprocess.run(
            [sys.executable, "-c", ACCESS_PROBE_SRC],
            cwd=case.repo,
            capture_output=True,
            text=True,
            timeout=60,
        )
        data = json.loads(proc.stdout.strip().splitlines()[-1])
    except Exception as exc:
        case._note("access probe %s failed: %s" % (label, exc))
        return None
    with open(os.path.join(case.dir, "product-probe-%s.json" % label), "w", encoding="utf-8") as fh:
        json.dump(data, fh, ensure_ascii=False, indent=1)
    return data


def mentions_admin_work(text: str) -> bool:
    """Whether a message treats administrator management as work, not scenery.

    The fixture already talks about an `admin` role, so a bare "admin" match
    would prove nothing. The user's addition was "管理员管理也没有"; evidence
    that it reached the planning/narration must mention administrators together
    with building or managing them.
    """
    if "管理员管理" in text:
        return True
    if "管理员" in text and any(
        word in text for word in ("页面", "实现", "补充", "添加", "增加", "列表", "管理")
    ):
        return True
    lowered = text.lower()
    if "admin" in lowered and any(
        word in lowered for word in ("page", "implement", "add ", "added", "management", "manage")
    ):
        return True
    return False


ADMIN_HEADING = re.compile(
    r"<(?:h[1-3]|title|section)[^>]*>[^<]*(?:管理员|admin)", re.IGNORECASE
)


def admin_management_pages(probe: dict | None) -> tuple[list[str], list[str], list[str]]:
    """Pages that actually show administrator management.

    The user did not name a route, so accept any reachable HTML page with an
    administrator heading/title that lists an administrator account, a
    dedicated `/admin` page, or an inline administrator section on /access.
    A bare table column label is not a management surface, which is why a
    heading (not just the word) is required. Return
    (reachable_pages, direct_pages, inline_pages).
    """
    if not probe:
        return [], [], []
    pages = probe.get("pages") or {}
    links = probe.get("links") or []

    def qualifies(path: str) -> bool:
        page = pages.get(path) or {}
        body = page.get("body") or ""
        return (
            page.get("status") == 200
            and "text/html" in str(page.get("type"))
            and ADMIN_HEADING.search(body) is not None
            and "Alice Chen" in body
        )

    reachable = [link for link in links if link not in ("/access", "/roles") and qualifies(link)]
    direct = [path for path in ("/admin",) if qualifies(path)]
    inline = ["/access"] if qualifies("/access") else []
    return reachable, direct, inline


def product_case(args, repeat: int = 0) -> int:
    case = Case("product-access", fixture=ACCESS_FIXTURE)
    result = case.result
    try:
        baseline = run_python_tests(case.repo, ["tests.test_access_layout"], timeout=120)
        with open(os.path.join(case.dir, "baseline-tests.log"), "w", encoding="utf-8") as fh:
            fh.write(baseline["out"] + "\n" + baseline["err"])
        result.check(
            "fixture_starts_red_on_the_layout_contract",
            baseline["code"] != 0 and "FAILED" in (baseline["out"] + baseline["err"]),
            "exit=%s" % baseline["code"],
        )

        result.check("daemon_ready", case.start_daemon(), case.repo)
        result.check("web_observer_ready", case.start_web(), str(case.web_port))
        a = case.tui("A")
        a.spawn()
        sid = case.wait_session()
        result.check("session_created", sid is not None, str(sid))
        seed = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=case.repo,
            capture_output=True,
            text=True,
            check=False,
        ).stdout.strip()

        a.type_and_enter(ACCESS_PROMPT, settle=0.6)
        marker = os.path.join(case.repo, ".slow_started")
        result.check(
            "baseline_suite_started",
            bool(wait_until(lambda: os.path.exists(marker), 180)),
            marker,
        )

        before = case.snapshot("before-steer") or {}
        result.check("turn_running_when_appended", before.get("status") == "running", str(before.get("status")))
        steer_at = time.time()
        a.type_and_enter(ACCESS_STEER, settle=0.4)
        admitted = wait_until(lambda: case.journal(ACCESS_STEER), 30)
        result.check("journal_admitted", bool(admitted), json.dumps(admitted, ensure_ascii=False))
        result.check(
            "journal_pending_while_the_turn_runs",
            bool(admitted and admitted[0]["state"] == "pending"),
            str(admitted and admitted[0]["state"]),
        )
        result.check(
            "frame_shows_saved",
            case.frame_has(a, "已保存 · 等待投递", 20),
            "A screen",
        )

        texts_before_delivery = len(case.assistant_texts())
        hit = case.wait_request_with(ACCESS_STEER, after=steer_at, timeout=600)
        result.check("next_model_round_received_it", bool(hit), json.dumps(hit))
        result.check(
            "frame_shows_delivered",
            case.frame_has(a, "已进入当前任务", 30),
            "A screen",
        )
        a.dump("steer-delivered")
        delivered = case.snapshot("steer-delivered") or {}
        result.check(
            "delivered_inside_the_same_turn",
            bool(
                hit
                and before.get("latest_turn_ordinal") is not None
                and delivered.get("latest_turn_ordinal") == before.get("latest_turn_ordinal")
            ),
            "before=%s delivered=%s"
            % (before.get("latest_turn_ordinal"), delivered.get("latest_turn_ordinal")),
        )
        occurrences = case.occurrences(ACCESS_STEER, after=steer_at)
        result.check(
            "steer_once_per_request",
            all(row["count"] <= 1 for row in occurrences),
            json.dumps(occurrences),
        )
        consumed = wait_until(
            lambda: (case.journal(ACCESS_STEER) or [{}])[0].get("state") == "consumed", 420
        )
        result.check("journal_consumed", bool(consumed), str(case.journal(ACCESS_STEER)))
        rows = case.user_message_rows(ACCESS_STEER)
        result.check("one_transcript_row", len(rows) == 1, str(len(rows)))
        result.check(
            "row_carries_journal_identity",
            bool(rows and case.journal(ACCESS_STEER) and rows[0].get("input_id") == case.journal(ACCESS_STEER)[0]["id"]),
            str(rows and rows[0].get("input_id")),
        )

        before_texts = texts_before_delivery
        terminal = case.wait_all_turns_terminal(steer_at, timeout=900)
        result.check("turn_terminal", bool(terminal), str(terminal and terminal.get("status")))
        a.dump("terminal")
        after_texts = case.assistant_texts()
        new_texts = after_texts[before_texts:]
        mentions = [text for text in new_texts if mentions_admin_work(text)]
        final_snapshot = case.snapshot("final-turn") or {}
        plan_text = json.dumps(final_snapshot.get("plan"), ensure_ascii=False)
        result.check(
            "reflected_in_narration_or_plan",
            bool(mentions) or mentions_admin_work(plan_text),
            "narration=%s plan=%s"
            % (json.dumps([text[-200:] for text in mentions], ensure_ascii=False), plan_text[:400]),
        )
        deliveries = case.tool_call_arguments(["update_goal", "deliver_answer"])
        delivery_text = json.dumps(
            deliveries[-1]["arguments"] if deliveries else {}, ensure_ascii=False
        )
        narration_covers = bool(after_texts and mentions_admin_work(after_texts[-1]))
        result.check(
            "final_answer_covers_the_added_requirement",
            narration_covers or mentions_admin_work(delivery_text),
            "final_text=%s delivery=%s"
            % (
                after_texts[-1][-200:] if after_texts else "",
                delivery_text[-400:],
            ),
        )

        probe = probe_access_pages(case, "after-terminal")
        reachable, direct, inline = admin_management_pages(probe)
        result.check(
            "admin_management_implemented",
            bool(reachable or direct or inline),
            "reachable=%s direct=%s inline=%s" % (reachable, direct, inline),
        )

        layout = run_python_tests(case.repo, ["tests.test_access_layout"], timeout=180)
        with open(os.path.join(case.dir, "layout-tests.log"), "w", encoding="utf-8") as fh:
            fh.write(layout["out"] + "\n" + layout["err"])
        result.check(
            "original_layout_contract_is_green",
            layout["code"] == 0,
            "exit=%s" % layout["code"],
        )
        suite = run_python_tests(case.repo, ["discover", "-s", "tests"], timeout=300)
        with open(os.path.join(case.dir, "suite-tests.log"), "w", encoding="utf-8") as fh:
            fh.write(suite["out"] + "\n" + suite["err"])
        result.check("project_suite_is_green", suite["code"] == 0, "exit=%s" % suite["code"])

        changed = subprocess.run(
            ["git", "diff", "--name-only", seed],
            cwd=case.repo,
            capture_output=True,
            text=True,
            check=False,
        ).stdout.split()
        for line in subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=case.repo,
            capture_output=True,
            text=True,
            check=False,
        ).stdout.splitlines():
            path = line[3:].strip()
            if path and path not in changed:
                changed.append(path)
        result.check(
            "contract_tests_were_not_edited_to_pass",
            not any(
                path in (
                    "tests/test_access_layout.py",
                    "tests/test_pages.py",
                    "tests/test_00_slow.py",
                )
                for path in changed
            ),
            json.dumps(changed),
        )
        result.check(
            "implementation_changed_the_project",
            any(not path.startswith("tests/") for path in changed),
            json.dumps(changed),
        )
        if probe:
            access_body = ((probe.get("pages") or {}).get("/access") or {}).get("body") or ""
            result.check(
                "access_page_links_navigation_and_stylesheet",
                "<nav" in access_body
                and 'rel="stylesheet"' in access_body
                and "/static/app.css" in access_body,
                "access page structure",
            )
    finally:
        case.finish()
    return 0 if all(check["ok"] for check in result.checks) else 1


SCENARIOS = {
    "sc1": sc1,
    "sc2": sc2,
    "sc3": sc3,
    "sc4": sc4,
    "sc5": sc5,
    "sc6": sc6,
    "sc7": sc7,
    "sc8": sc8,
    "product": product_case,
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("case", choices=sorted(SCENARIOS) + ["all"])
    parser.add_argument("--repeat", type=int, default=1)
    args = parser.parse_args()
    names = sorted(SCENARIOS) if args.case == "all" else [args.case]
    worst = 0
    for name in names:
        for repeat in range(args.repeat):
            print("\n===== %s repeat %d =====" % (name, repeat + 1))
            worst = max(worst, SCENARIOS[name](args, repeat))
    return worst


if __name__ == "__main__":
    sys.exit(main())
