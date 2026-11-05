#!/usr/bin/env python3
"""Real-PTY dogfood for the CodeLeveler TUI edit-diff surface.

This drives the REAL `leveler` binary on a REAL pseudo-terminal, with the real
runtime, reducer and renderer, and reads the frames back from a small VT-100
model of the screen. It is not a TestBackend: the bytes it asserts on are the
bytes a terminal emulator receives.

Two provider layers are supported, and they are never conflated:

  --provider mock      a local OpenAI-wire HTTP server serving scripted SSE
                       frames. The tool call is real: the runtime really runs
                       `write_file` and the patch it reports is the applied one.
  --provider real      the machine's own provider config (copied into the
                       isolated LEVELER_HOME). This is the live-model layer.

Usage:
    python3 scripts/pty_dogfood.py --provider mock
    python3 scripts/pty_dogfood.py --provider real
    python3 scripts/pty_dogfood.py --provider mock --only mouse

Evidence (frames, event log, assertions) is written to target/pty-dogfood/.
Exit code 0 means every assertion in every selected scenario held.
"""

from __future__ import annotations

import argparse
import codecs
import fcntl
import http.server
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("LEVELER_BIN") or os.path.join(ROOT, "target", "debug", "leveler")
EVIDENCE = os.path.join(ROOT, "target", "pty-dogfood")

# The daemon path is the one users run: the TUI attaches to a socket runtime
# that owns detached work. Scenarios that need connection faults are meaningless
# in-process, so the transport is selected once per run.
TRANSPORT = "in-process"

# ---------------------------------------------------------------------------
# Minimal VT-100 screen model
# ---------------------------------------------------------------------------


def cell_width(ch: str) -> int:
    """Terminal cells for one character (East-Asian wide + emoji => 2)."""
    if unicodedata.east_asian_width(ch) in ("W", "F"):
        return 2
    if unicodedata.combining(ch):
        return 0
    code = ord(ch)
    if 0x1F300 <= code <= 0x1FAFF or 0x2600 <= code <= 0x27BF:
        return 2
    return 1


class Screen:
    """The visible terminal: rows of cells, written by the SGR/CSI subset a
    ratatui frame actually emits (absolute cursor moves + printed text)."""

    def __init__(self, cols: int, rows: int):
        self.cols = cols
        self.rows = rows
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = 0
        self.col = 0
        self._pending = ""
        self._state = "text"
        self._params = ""

    def reset(self, cols: int, rows: int):
        self.cols, self.rows = cols, rows
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = self.col = 0

    def resize(self, cols: int, rows: int):
        grid = [[" "] * cols for _ in range(rows)]
        for r in range(min(rows, self.rows)):
            for c in range(min(cols, self.cols)):
                grid[r][c] = self.grid[r][c]
        self.cols, self.rows, self.grid = cols, rows, grid
        self.row = min(self.row, rows - 1)
        self.col = min(self.col, cols - 1)

    def _put(self, ch: str):
        w = cell_width(ch)
        if w == 0:
            return
        if self.col + w > self.cols:
            self.col = 0
            self.row = min(self.row + 1, self.rows - 1)
        if 0 <= self.row < self.rows and 0 <= self.col < self.cols:
            self.grid[self.row][self.col] = ch
            for k in range(1, w):
                if self.col + k < self.cols:
                    self.grid[self.row][self.col + k] = ""
        self.col += w

    def _csi(self, final: str, body: str):
        private = body.startswith("?")
        if private:
            body = body[1:]
        nums = [int(p) if p.isdigit() else 0 for p in body.split(";") if p != ""] or [0]
        if final in "Hf":
            r = (nums[0] or 1) - 1
            c = (nums[1] if len(nums) > 1 else 1) or 1
            self.row = max(0, min(r, self.rows - 1))
            self.col = max(0, min(c - 1, self.cols - 1))
        elif final == "A":
            self.row = max(0, self.row - (nums[0] or 1))
        elif final == "B":
            self.row = min(self.rows - 1, self.row + (nums[0] or 1))
        elif final == "C":
            self.col = min(self.cols - 1, self.col + (nums[0] or 1))
        elif final == "D":
            self.col = max(0, self.col - (nums[0] or 1))
        elif final == "G":
            self.col = max(0, min((nums[0] or 1) - 1, self.cols - 1))
        elif final == "d":
            self.row = max(0, min((nums[0] or 1) - 1, self.rows - 1))
        elif final == "J":
            mode = nums[0] if nums else 0
            if mode == 2 or mode == 3:
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
            elif mode == 0:
                for c in range(self.col, self.cols):
                    self.grid[self.row][c] = " "
                for r in range(self.row + 1, self.rows):
                    self.grid[r] = [" "] * self.cols
        elif final == "K":
            mode = nums[0] if nums else 0
            if mode == 0:
                for c in range(self.col, self.cols):
                    self.grid[self.row][c] = " "
            elif mode == 1:
                for c in range(0, self.col + 1):
                    self.grid[self.row][c] = " "
            else:
                self.grid[self.row] = [" "] * self.cols
        elif final in "ST":
            self.grid = [[" "] * self.cols for _ in range(self.rows)]
        # m (SGR), h/l (modes), n/q/r (reports) carry nothing a frame assert needs.

    def feed(self, text: str):
        text = self._pending + text
        self._pending = ""
        i = 0
        n = len(text)
        while i < n:
            ch = text[i]
            if self._state == "text":
                if ch == "\x1b":
                    self._state = "esc"
                    i += 1
                    continue
                if ch == "\r":
                    self.col = 0
                elif ch == "\n":
                    self.row = min(self.row + 1, self.rows - 1)
                elif ch == "\b":
                    self.col = max(0, self.col - 1)
                elif ch == "\t":
                    self.col = min(self.cols - 1, (self.col // 8 + 1) * 8)
                elif ch == "\x07":
                    pass
                else:
                    self._put(ch)
                i += 1
                continue
            if self._state == "esc":
                if ch == "[":
                    self._state = "csi"
                    self._params = ""
                    i += 1
                    continue
                if ch == "]":
                    self._state = "osc"
                    i += 1
                    continue
                # Two-byte escape (e.g. ESC ( B, ESC 7)
                self._state = "text"
                i += 1
                continue
            if self._state == "osc":
                if ch == "\x07":
                    self._state = "text"
                elif ch == "\x1b":
                    self._state = "osc_esc"
                i += 1
                continue
            if self._state == "osc_esc":
                self._state = "text"
                i += 1
                continue
            if self._state == "csi":
                if "0" <= ch <= "9" or ch in ";?":
                    self._params += ch
                    i += 1
                    continue
                if ch == "\x1b":
                    # A truncated sequence: restart escape handling.
                    self._state = "esc"
                    i += 1
                    continue
                self._csi(ch, self._params)
                self._state = "text"
                i += 1
                continue
            i += 1
        # An escape split across two reads keeps its partial state.
        if self._state != "text" and n and text.endswith("\x1b"):
            self._state = "text"
            self._pending = "\x1b"

    def lines(self) -> list[str]:
        return ["".join(c for c in row).rstrip() for row in self.grid]

    def text(self) -> str:
        return "\n".join(self.lines())

    def find(self, needle: str) -> tuple[int, int] | None:
        for r, line in enumerate(self.lines()):
            c = line.find(needle)
            if c >= 0:
                return (r, c)
        return None


# ---------------------------------------------------------------------------
# PTY session
# ---------------------------------------------------------------------------


class Pty:
    def __init__(self, argv: list[str], env: dict[str, str], cols: int, rows: int):
        self.cols, self.rows = cols, rows
        self.screen = Screen(cols, rows)
        # A read can split a multi-byte character; an incremental decoder keeps
        # the tail instead of mangling the last glyph of a frame.
        self._decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.master, slave = pty.openpty()
        self._winsize(cols, rows, fd=slave)
        self.raw = bytearray()
        self.events: list[tuple[float, str]] = []
        self.scrolls = 0
        self._deadline = time.time() + 120
        try:
            self.proc = subprocess.Popen(
                argv,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                env=env,
                start_new_session=True,
                close_fds=True,
            )
        finally:
            os.close(slave)
        self._stop = threading.Event()
        self._reader = threading.Thread(target=self._pump, daemon=True)
        self._reader.start()

    def _winsize(self, cols: int, rows: int, fd: int | None = None):
        fd = self.master if fd is None else fd
        fcntl.ioctl(fd, termios_tiocswinsz(), struct.pack("HHHH", rows, cols, 0, 0))

    def _pump(self):
        while not self._stop.is_set():
            try:
                r, _, _ = select.select([self.master], [], [], 0.05)
            except (OSError, ValueError):
                break
            if not r:
                continue
            try:
                data = os.read(self.master, 1 << 16)
            except OSError:
                break
            if not data:
                break
            self.raw.extend(data)
            text = self._decoder.decode(data)
            if not text:
                continue
            self.events.append((time.time(), text))
            self.screen.feed(text)

    def alive(self) -> bool:
        return self.proc.poll() is None

    def write(self, data: str):
        os.write(self.master, data.encode())

    def key(self, *seqs: str):
        for seq in seqs:
            self.write(seq)
            time.sleep(0.05)

    def click(self, row: int, col: int, button: int = 0):
        """SGR-1006 mouse: press then release on the same cell."""
        self.write("\x1b[<%d;%d;%dM" % (button, col + 1, row + 1))
        time.sleep(0.25)
        self.write("\x1b[<%d;%d;%dm" % (button, col + 1, row + 1))
        time.sleep(0.35)

    def wheel(self, up: bool, row: int = 10, col: int = 5, times: int = 1):
        code = 64 if up else 65
        for _ in range(times):
            self.write("\x1b[<%d;%d;%dM" % (code, col + 1, row + 1))
            time.sleep(0.05)
        time.sleep(0.3)

    def drag(self, r0: int, c0: int, r1: int, c1: int):
        self.write("\x1b[<0;%d;%dM" % (c0 + 1, r0 + 1))
        time.sleep(0.15)
        steps = 8
        for k in range(1, steps + 1):
            r = r0 + (r1 - r0) * k // steps
            c = c0 + (c1 - c0) * k // steps
            self.write("\x1b[<32;%d;%dM" % (c + 1, r + 1))
            time.sleep(0.06)
        self.write("\x1b[<0;%d;%dm" % (c1 + 1, r1 + 1))
        time.sleep(0.4)

    def resize(self, cols: int, rows: int):
        self.cols, self.rows = cols, rows
        self._winsize(cols, rows)
        self.screen.resize(cols, rows)
        os.kill(self.proc.pid, signal.SIGWINCH)
        time.sleep(0.6)

    def wait_for(self, needle: str, timeout: float = 20.0):
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.screen.text():
                return True
            if not self.alive():
                return needle in self.screen.text()
            time.sleep(0.05)
        return needle in self.screen.text()

    def wait_stable(self, quiet: float = 0.8, timeout: float = 60.0):
        end = time.time() + timeout
        last = len(self.raw)
        stamp = time.time()
        while time.time() < end:
            time.sleep(0.1)
            if len(self.raw) != last:
                last = len(self.raw)
                stamp = time.time()
            elif time.time() - stamp >= quiet:
                return True
            if not self.alive():
                return True
        return False

    def close(self):
        self._stop.set()
        try:
            os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except Exception:
            pass
        try:
            os.close(self.master)
        except Exception:
            pass


def termios_tiocswinsz() -> int:
    # TIOCSWINSZ differs per platform; derive it from termios where possible.
    import termios

    return getattr(termios, "TIOCSWINSZ", 0x80047410 if sys.platform == "darwin" else 0x5414)


# ---------------------------------------------------------------------------
# Scripted OpenAI-wire provider (mock layer)
# ---------------------------------------------------------------------------


class MockProvider:
    """Serves scripted SSE frames per request, in order.

    The script is expressed as a list of "turns"; each turn is the set of SSE
    frames for ONE model request. Requests beyond the script repeat the last
    turn so a smoke run cannot hang on an unexpected extra round.
    """

    def __init__(self):
        self.turns: list[list[str]] = []
        self.requests: list[str] = []
        self.calls = 0
        self.lock = threading.Lock()
        provider = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_POST(self):
                length = int(self.headers.get("content-length") or 0)
                body = self.rfile.read(length).decode("utf-8", "replace") if length else ""
                with provider.lock:
                    index = min(provider.calls, len(provider.turns) - 1)
                    provider.calls += 1
                    provider.requests.append(body)
                frames = provider.turns[index] if provider.turns else []
                payload = "".join(
                    "data: %s\n\n" % frame for frame in frames
                ) + "data: [DONE]\n\n"
                data = payload.encode()
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.send_header("content-length", str(len(data)))
                self.send_header("connection", "close")
                self.end_headers()
                try:
                    self.wfile.write(data)
                except Exception:
                    pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def turn_requests(self) -> int:
        """Requests that carry tools — a real agent turn, not an assist call.

        The prompt-assist / recap helpers are separate, tool-less requests; they
        cannot run a tool or write a file, so they are not what "did the reload
        re-execute the turn?" asks about.
        """
        with self.lock:
            bodies = list(self.requests)
        count = 0
        for body in bodies:
            try:
                req = json.loads(body)
            except Exception:
                continue
            if req.get("tools"):
                count += 1
        return count

    def base_url(self) -> str:
        return "http://127.0.0.1:%d/v1" % self.port

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


def tool_turn(name: str, arguments: str) -> list[str]:
    return [
        json.dumps(
            {
                "choices": [
                    {
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {"name": name, "arguments": arguments},
                                }
                            ]
                        }
                    }
                ]
            }
        ),
        json.dumps({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ]


def text_turn(text: str) -> list[str]:
    return [
        json.dumps({"choices": [{"delta": {"content": text}}]}),
        json.dumps({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ]


# ---------------------------------------------------------------------------
# Environment
# ---------------------------------------------------------------------------


def isolated_env(provider: str, mock: MockProvider | None, short: bool = False):
    # The daemon's Unix socket lives under LEVELER_HOME/run/sockets and must
    # stay below SUN_LEN (~104 bytes on macOS). The default macOS temp root is
    # long enough to overflow it, so daemon scenarios opt into a short root.
    if short:
        root = tempfile.mkdtemp(prefix="lv-", dir="/tmp")
    else:
        root = tempfile.mkdtemp(prefix="leveler-pty-")
    home = os.path.join(root, "home")
    repo = os.path.join(root, "repo")
    os.makedirs(home)
    os.makedirs(os.path.join(repo, ".leveler"))
    with open(os.path.join(repo, ".leveler", "config.yaml"), "w") as fh:
        fh.write("{}\n")
    subprocess.run(["git", "init", "-q"], cwd=repo, check=False)
    subprocess.run(["git", "-c", "user.email=a@b", "-c", "user.name=t", "commit",
                    "-q", "--allow-empty", "-m", "seed"], cwd=repo, check=False)
    cfg = os.path.join(home, "config.toml")
    if provider == "mock":
        assert mock is not None
        with open(cfg, "w") as fh:
            fh.write(
                'default_model = "mock-a"\nlang = "zh"\n\n'
                '[providers.mock]\nprotocol = "openai_chat"\nbase_url = "%s"\n\n'
                '[models."mock-a"]\nprovider = "mock"\nmodel_id = "model-a"\n'
                "context_window = 200000\nmax_output_tokens = 8192\n"
                % mock.base_url()
            )
    else:
        source = os.path.expanduser("~/.leveler/config.toml")
        shutil.copyfile(source, cfg)
    env = dict(os.environ)
    env.update(
        {
            "TERM": "xterm-256color",
            "LEVELER_HOME": home,
            "COLORTERM": "truecolor",
            "LEVELER_LOG": "warn",
        }
    )
    return root, home, repo, env


def spawn(
    env: dict[str, str],
    repo: str,
    cols: int,
    rows: int,
    extra: list[str],
    bin_path: str | None = None,
) -> Pty:
    argv = [bin_path or BIN, "--repo", repo, "tui", "--auto-approve"]
    if TRANSPORT == "in-process":
        argv.append("--in-process")
    argv += ["--permission", "full-access"] + extra
    return Pty(argv, env, cols, rows)


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------


class Report:
    def __init__(self, layer: str):
        self.layer = layer
        self.results: list[dict] = []
        self.failures = 0
        self.blocked = 0

    def check(self, scenario: str, name: str, ok: bool, detail: str = ""):
        self.results.append(
            {"scenario": scenario, "check": name, "ok": bool(ok), "detail": detail}
        )
        if not ok:
            self.failures += 1
        print(("  ok   " if ok else "  FAIL ") + f"[{scenario}] {name}" + (f" — {detail}" if detail and not ok else ""))

    def block(self, scenario: str, name: str, detail: str):
        """A required behavior the harness cannot reach. Not a pass and not a
        product failure: the precondition itself is unavailable, and the
        reason is recorded so the gap cannot be mistaken for coverage."""
        self.results.append(
            {
                "scenario": scenario,
                "check": name,
                "ok": None,
                "blocked": True,
                "detail": detail,
            }
        )
        self.blocked += 1
        print(f"  BLOCKED [{scenario}] {name} — {detail}")

    def dump(self, name: str, pty: Pty):
        path = os.path.join(EVIDENCE, name + ".txt")
        with open(path, "w") as fh:
            fh.write(pty.screen.text())
            fh.write("\n")
        raw = os.path.join(EVIDENCE, name + ".raw")
        with open(raw, "wb") as fh:
            fh.write(bytes(pty.raw))
        return path

    def save(self):
        path = os.path.join(EVIDENCE, "report-%s.json" % self.layer)
        with open(path, "w") as fh:
            json.dump(
                {
                    "layer": self.layer,
                    "failures": self.failures,
                    "blocked": self.blocked,
                    "results": self.results,
                },
                fh,
                indent=2,
            )
        return path


CONTENT_LINES = 103


def cookie_content(n: int = CONTENT_LINES) -> str:
    return "".join("    expectCookie%d(t)\n" % i for i in range(1, n + 1))


LONG_LINE = (
    "    expectLongCookie(request, response, cookieName, expectedValue, ttl, domain, path, secure, httponly)"
)


def mixed_content(n: int = CONTENT_LINES) -> str:
    """`n` short lines with one very long line at 5 — soft wrapping included."""
    lines = []
    for i in range(1, n + 1):
        lines.append(LONG_LINE + "\n" if i == 5 else "    expectCookie%d(t)\n" % i)
    return "".join(lines)


def create_file_args(path: str = "cookies_test.go", n: int = CONTENT_LINES) -> str:
    return json.dumps({"path": path, "content": cookie_content(n)})


# ---------------------------------------------------------------------------
# Scenarios
# ---------------------------------------------------------------------------


class Env:
    """One isolated LEVELER_HOME + repo, plus the process environment."""

    def __init__(self, provider: str, mock, short: bool = False):
        self.root, self.home, self.repo, self.env = isolated_env(provider, mock, short)


def make_env(provider: str, mock) -> "Env":
    return Env(provider, mock)


def boot(
    report: Report,
    env: "Env",
    cols=120,
    rows=44,
    extra=None,
    bin_path: str | None = None,
) -> Pty:
    pty_ = spawn(env.env, env.repo, cols, rows, extra or [], bin_path)
    ok = pty_.wait_for("leveler", timeout=60) or pty_.wait_stable(1.0, 60)
    report.check(
        "boot",
        "the real binary paints a frame on a real PTY at %dx%d" % (cols, rows),
        ok,
        pty_.screen.text()[:400],
    )
    return pty_


def ask(pty_: Pty, prompt: str):
    pty_.write(prompt)
    time.sleep(0.2)
    pty_.key("\r")
    pty_.wait_stable(1.2, 180)


def diff_rows(pty_: Pty) -> list[str]:
    return [ln for ln in pty_.screen.lines() if "│ +" in ln or "│ -" in ln]


def flat(pty_: Pty) -> str:
    """Screen text with all whitespace removed — soft-wrapped tokens survive."""
    return "".join(pty_.screen.text().split())


EDIT_HEAD_WORDS = ("写入文件", "应用补丁", "编辑文件", "Write ", "Edit ", "Apply ")


def find_edit_head(pty_: Pty, path: str):
    """The edit NODE's head row — never a prose mention of the same path."""
    for r, line in enumerate(pty_.screen.lines()):
        if path in line and any(w in line for w in EDIT_HEAD_WORDS):
            return (r, line.find(path))
    return None


def scroll_until(pty_: Pty, needle: str, up: bool = True, tries: int = 30) -> bool:
    for _ in range(tries):
        if needle in pty_.screen.text():
            return True
        pty_.wheel(up=up, row=20, col=10, times=3)
    return needle in pty_.screen.text()


def scenario_default_display(report: Report, provider: str, mock):
    """A: a created 103-line file is fully expanded by default."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 130)
    try:
        ask(pty_, "create cookies_test.go with 103 lines")
        pty_.wait_for("expectCookie103(t)", 180)
        pty_.wait_stable(1.0, 30)
        view = pty_.screen.text()
        report.dump("A-120x130-expanded", pty_)
        rows = diff_rows(pty_)
        report.check(
            "A",
            "every applied line is painted on one frame (103 rows)",
            len(rows) == CONTENT_LINES,
            "painted %d diff rows" % len(rows),
        )
        report.check(
            "A",
            "the first applied line is on screen with gutter number 1",
            re.search(r"^\s*1 │ \+\s+expectCookie1\(t\)", view, re.M) is not None,
            view[:600],
        )
        report.check(
            "A",
            "the last applied line is on screen with gutter number 103",
            re.search(r"^\s*103 │ \+\s+expectCookie103\(t\)", view, re.M) is not None,
            view[-600:],
        )
        report.check(
            "A",
            "no mechanical elision hint exists",
            "还有" not in view and "lines hidden" not in view,
            view,
        )
        pty_.write("zzz")
        pty_.wait_stable(0.6, 10)
        typed = pty_.screen.text()
        report.dump("A-input-after-diff", pty_)
        report.check("A", "the input area still accepts text", "zzz" in typed, typed[-300:])
        pty_.write("\x7f\x7f\x7f")
        pty_.wait_stable(0.5, 10)

        pty_.resize(120, 44)
        pty_.wait_stable(1.0, 30)
        pty_.wheel(up=False, row=20, col=10, times=8)
        tail = pty_.screen.text()
        report.dump("A-120x44-scrolled-to-tail", pty_)
        report.check(
            "A",
            "the last applied line is reachable in a short viewport",
            "expectCookie103(t)" in tail,
            tail[-600:],
        )
        head_ok = scroll_until(pty_, "expectCookie1(t)", up=True, tries=40)
        head = pty_.screen.text()
        report.dump("A-120x44-scrolled-to-head", pty_)
        report.check(
            "A",
            "the first applied line is reachable in a short viewport",
            head_ok,
            head[:600],
        )
        return pty_
    finally:
        pty_.close()


def scenario_mouse(report: Report, provider: str, mock):
    """B: click folds/expands this edit only, without moving the reading anchor."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 130)
    try:
        ask(pty_, "create cookies_test.go with 103 lines")
        pty_.wait_for("expectCookie103(t)", 180)
        pty_.wait_stable(1.0, 30)
        expanded = pty_.screen.text()
        report.dump("B-120x130-two-edits-expanded", pty_)
        head = pty_.screen.find("写入文件  cookies_test.go")
        report.check("B", "the edit head row is on screen", head is not None)
        if head is None:
            return pty_
        pty_.click(head[0], 8)
        folded = pty_.screen.text()
        report.dump("B-120x130-folded", pty_)
        report.check(
            "B",
            "a click folds the diff into one summary row",
            "expectCookie3(t)" not in folded and "cookies_test.go" in folded,
            folded[:800],
        )
        report.check(
            "B",
            "the folded row states the applied totals",
            re.search(r"\+103/-0", folded) is not None,
            folded[:800],
        )
        pty_.click(head[0], 8)
        reopened = pty_.screen.text()
        report.dump("B-120x130-reopened", pty_)
        report.check(
            "B",
            "a second click restores the same frame",
            reopened.strip() == expanded.strip(),
            reopened[:800],
        )
        # A different edit node must not move THIS edit's fold state.
        other = pty_.screen.find("写入文件  second.go")
        report.check("B", "the second edit has its own node", other is not None)
        if other is not None:
            pty_.click(other[0], 8)
            after = pty_.screen.text()
            report.dump("B-120x130-other-edit-folded", pty_)
            report.check(
                "B",
                "folding the other edit leaves this one expanded",
                "expectCookie3(t)" in after and "secondLine3" not in after,
                after[:800],
            )
            pty_.click(other[0], 8)
            pty_.wait_stable(0.6, 20)

        # A short viewport proves the fold keeps the reading anchor: the head
        # row stays on the same screen row it was clicked on.
        pty_.resize(120, 44)
        pty_.wait_stable(1.0, 30)
        found = scroll_until(pty_, "写入文件  cookies_test.go", up=True, tries=40)
        report.check("B", "the edit head is reachable in a short viewport", found)
        if found:
            row_before = pty_.screen.find("写入文件  cookies_test.go")[0]
            report.dump("B-120x44-before-fold", pty_)
            pty_.click(row_before, 8)
            after_fold = pty_.screen.find("写入文件  cookies_test.go")
            report.dump("B-120x44-after-fold", pty_)
            report.check(
                "B",
                "folding does not move the head off its screen row",
                after_fold is not None and after_fold[0] == row_before,
                "before row %d, after %s" % (row_before, after_fold),
            )
            report.check(
                "B",
                "no cookie rows remain after folding in a short viewport",
                not any("expectCookie" in ln for ln in pty_.screen.lines()),
                str([ln for ln in pty_.screen.lines() if "expectCookie" in ln][:3]),
            )
        return pty_
    finally:
        pty_.close()


def scenario_keyboard(report: Report, provider: str, mock):
    """C: Ctrl+O folds the latest edit node and leaves earlier nodes alone."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 130)
    try:
        ask(pty_, "run echo hi then create cookies_test.go")
        pty_.wait_for("expectCookie103(t)", 180)
        pty_.wait_stable(1.0, 30)
        report.dump("C-before-ctrl-o", pty_)
        command_row = "执行了 1 个命令"
        had_command = command_row in pty_.screen.text()
        report.check("C", "an unrelated command node is on screen", had_command)
        pty_.key("\x0f")
        pty_.wait_stable(0.8, 20)
        folded = pty_.screen.text()
        report.dump("C-ctrl-o-folded", pty_)
        report.check(
            "C",
            "Ctrl+O folds the latest edit-only node",
            not diff_rows(pty_) and re.search(r"\+103/-0", folded) is not None,
            folded[:800],
        )
        report.check(
            "C",
            "the command node is untouched by Ctrl+O",
            (command_row in folded) == had_command,
            folded[:800],
        )
        pty_.key("\x0f")
        pty_.wait_stable(0.8, 20)
        reopened = pty_.screen.text()
        report.dump("C-ctrl-o-reopened", pty_)
        report.check(
            "C",
            "a second Ctrl+O restores the diff body",
            len(diff_rows(pty_)) > 0 and "expectCookie" in reopened,
            reopened[:800],
        )
        pty_.write("zzz")
        pty_.wait_stable(0.6, 10)
        typed = pty_.screen.text()
        report.dump("C-input-after-ctrl-o", pty_)
        report.check("C", "the input area still accepts text", "zzz" in typed, typed[-400:])
        pty_.key("\x1b")
        pty_.write("\x7f\x7f\x7f")
        pty_.wait_stable(0.5, 10)
        return pty_
    finally:
        pty_.close()


def scenario_history(report: Report, provider: str, mock):
    """D: a persisted edit is recovered by a real history reload, not re-run."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 130)
    try:
        ask(pty_, "create cookies_test.go with 103 lines")
        pty_.wait_for("expectCookie103(t)", 180)
        pty_.wait_stable(1.0, 30)
        report.dump("D-1-live", pty_)
        requests_live = mock.calls if mock is not None else -1
        turn_live = mock.turn_requests() if mock is not None else -1
        # A fresh conversation empties the surface; the finished one is durable.
        ask(pty_, "/new")
        pty_.wait_stable(1.0, 30)
        requests_before = mock.calls if mock is not None else -1
        turn_before = mock.turn_requests() if mock is not None else -1
        print(
            "    provider requests: live=%d new-session=%d (turns %d -> %d)"
            % (requests_live, requests_before, turn_live, turn_before)
        )
        cleared = pty_.screen.text()
        report.dump("D-2-new-session", pty_)
        report.check(
            "D",
            "a new session paints no edit from the previous one",
            "expectCookie3(t)" not in cleared,
            cleared[:600],
        )
        ask(pty_, "/sessions")
        pty_.wait_stable(0.6, 20)
        report.dump("D-3-sessions", pty_)
        row = pty_.screen.find("create cookies_test.go")
        report.check("D", "the saved session is listed", row is not None)
        if row is None:
            return pty_
        pty_.click(row[0], 6)
        pty_.key("\r")
        pty_.wait_stable(1.2, 60)
        reloaded = pty_.screen.text()
        report.dump("D-4-reloaded", pty_)
        report.check(
            "D",
            "the reloaded history shows the applied diff expanded",
            len(diff_rows(pty_)) == CONTENT_LINES,
            "painted %d rows" % len(diff_rows(pty_)),
        )
        report.check(
            "D",
            "the reloaded diff is the same applied patch",
            re.search(r"^\s*1 │ \+\s+expectCookie1\(t\)", reloaded, re.M) is not None
            and re.search(r"^\s*103 │ \+\s+expectCookie103\(t\)", reloaded, re.M)
            is not None,
            reloaded[:600],
        )
        if mock is not None:
            print(
                "    provider requests after reload: %d (turns %d)"
                % (mock.calls, mock.turn_requests())
            )
            for k, body in enumerate(mock.requests):
                try:
                    req = json.loads(body)
                except Exception:
                    continue
                msgs = req.get("messages") or []
                tail = [(m.get("role"), str(m.get("content"))[:90], bool(m.get("tool_calls"))) for m in msgs[-3:]]
                print("      req%d: %d msgs, tools=%s tail=%s" % (k, len(msgs), len(req.get("tools") or []), tail))
            report.check(
                "D",
                "history reload re-executes no agent turn (no tool-carrying request)",
                mock.turn_requests() == turn_before,
                "turn requests %d -> %d" % (turn_before, mock.turn_requests()),
            )
            report.check(
                "D",
                "any request after a reload is a tool-less assist/recap request",
                all(
                    not (json.loads(b).get("tools"))
                    for b in mock.requests[requests_before:]
                    if b.strip().startswith("{")
                ),
                str([b[:80] for b in mock.requests[requests_before:]]),
            )
        head = pty_.screen.find("写入文件  cookies_test.go")
        report.check("D", "the historical edit node is clickable", head is not None)
        if head is not None:
            pty_.click(head[0], 8)
            folded = pty_.screen.text()
            report.dump("D-5-reloaded-folded", pty_)
            report.check(
                "D",
                "a historical node folds on click like a live one",
                not diff_rows(pty_) and re.search(r"\+103/-0", folded) is not None,
                folded[:800],
            )
            pty_.click(head[0], 8)
            pty_.wait_stable(0.8, 20)
            report.dump("D-6-reloaded-reopened", pty_)
            visible = len(diff_rows(pty_))
            tail = scroll_until(pty_, "expectCookie103(t)", up=False, tries=40)
            report.dump("D-7-reopened-tail", pty_)
            report.check(
                "D",
                "a historical node reopens on click",
                visible >= CONTENT_LINES - 2 and tail,
                "painted %d rows, tail reachable=%s" % (visible, tail),
            )
        return pty_
    finally:
        pty_.close()


def scenario_sizes(report: Report, provider: str, mock):
    """E: layout at 24/40/80/120 columns, incl. resize and soft wrap."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 44)
    try:
        ask(pty_, "create cookies_test.go with 103 lines")
        pty_.wait_for("expectCookie103(t)", 180)
        pty_.wait_stable(1.0, 30)
        for cols, rows in [(24, 30), (40, 30), (80, 30), (120, 44)]:
            pty_.resize(cols, rows)
            pty_.wait_stable(0.8, 30)
            frame = pty_.screen.text()
            report.dump("E-%dx%d" % (cols, rows), pty_)
            overflow = [ln for ln in pty_.screen.lines() if len(ln) > cols]
            report.check(
                "E",
                "%dx%d paints without overflowing the pane" % (cols, rows),
                not overflow,
                str(overflow[:3]),
            )
            report.check(
                "E",
                "%dx%d still renders the diff body" % (cols, rows),
                "expectCookie" in flat(pty_) or "ctCookie" in flat(pty_),
                frame[:400],
            )
            report.check(
                "E",
                "%dx%d keeps real gutter numbers" % (cols, rows),
                "│ +" in frame and re.search(r"\b10[0-3] \│ \+", frame) is not None,
                frame[:400],
            )
        # Scroll inside the narrowest layout: response must stay bounded.
        pty_.resize(24, 30)
        pty_.wait_stable(0.8, 20)
        start = time.time()
        pty_.wheel(up=True, row=10, col=5, times=20)
        pty_.wait_stable(0.6, 20)
        report.dump("E-24x30-scrolled", pty_)
        report.check(
            "E",
            "24x30 answers a scroll burst without stalling",
            time.time() - start < 15,
            "%.1fs" % (time.time() - start),
        )
        return pty_
    finally:
        pty_.close()


def clipboard_read() -> str | None:
    try:
        return subprocess.run(["pbpaste"], capture_output=True, text=True, timeout=5).stdout
    except Exception:
        return None


def clipboard_write(text: str) -> None:
    try:
        subprocess.run(["pbcopy"], input=text, text=True, timeout=5)
    except Exception:
        pass


def scenario_copy(report: Report, provider: str, mock):
    """Selecting diff rows in a real PTY copies the source text, not the chrome."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 130)
    saved = clipboard_read()
    try:
        ask(pty_, "create cookies_test.go with 103 lines")
        pty_.wait_for("expectCookie103(t)", 180)
        pty_.wait_stable(1.0, 30)
        report.dump("F-before-drag", pty_)
        first = pty_.screen.find("expectCookie4(t)")
        last = pty_.screen.find("expectCookie7(t)")
        report.check("F", "the rows to select are on screen", first is not None and last is not None)
        if first is None or last is None:
            return pty_
        clipboard_write("")
        # Start in the row's gutter (column 0) so the whole first line is
        # covered, and end past the last line's text on the right.
        pty_.drag(first[0], 2, last[0], 100)
        pty_.wait_stable(1.0, 30)
        report.dump("F-after-drag", pty_)
        copied = clipboard_read()
        expected = "\n".join(
            [LONG_LINE if i == 5 else "    expectCookie%d(t)" % i for i in range(4, 8)]
        )
        print("    clipboard after drag: %r" % copied)
        report.check("F", "the drag produced clipboard content", bool(copied), repr(copied))
        report.check(
            "F",
            "the clipboard holds exactly the selected source lines",
            copied == expected,
            "got %r, want %r" % (copied, expected),
        )
        report.check(
            "F",
            "no line number, gutter or +/- marker reached the clipboard",
            copied is not None
            and "│" not in copied
            and not re.search(r"^\s*\d+", copied, re.M)
            and "+ " not in copied,
            repr(copied),
        )
        notice = pty_.screen.text()
        report.check(
            "F",
            "the UI reports the copy",
            "复制" in notice or "cop" in notice.lower(),
            notice[-300:],
        )
        report.check(
            "F",
            "a soft-wrapped long line copies as ONE logical line",
            copied is not None
            and LONG_LINE in copied
            and copied.count("\n") == expected.count("\n"),
            "lines=%s" % (copied.count("\n") if copied else None),
        )

        # The complete applied patch is a DIFFERENT entry: a unified diff a
        # reader can apply, not the selected source text.
        clipboard_write("")
        pty_.write("/copy diff")
        pty_.key("\r")
        pty_.wait_stable(1.2, 45)
        report.dump("F-after-copy-diff", pty_)
        patch = clipboard_read()
        print("    clipboard after /copy diff: %r" % (patch or "")[:400])
        report.check("F", "/copy diff put a patch on the clipboard", bool(patch), repr(patch))
        report.check(
            "F",
            "/copy diff is a unified diff (--- / +++ / @@), not the selection",
            patch is not None
            and patch.startswith("--- ")
            and "+++ b/cookies_test.go" in patch
            and "@@" in patch,
            repr((patch or "")[:200]),
        )
        report.check(
            "F",
            "/copy diff carries the whole applied content",
            patch is not None
            and "expectCookie1(t)" in patch
            and "expectCookie103(t)" in patch
            and LONG_LINE.strip() in patch,
            "len=%d" % len(patch or ""),
        )
        return pty_
    finally:
        if saved is not None:
            clipboard_write(saved)
        pty_.close()


def shapes_content(n: int = 12) -> str:
    lines = ["package shapes", ""]
    for i in range(1, n + 1):
        lines.append("func shape%02d() {}" % i)
    return "\n".join(lines) + "\n"


def shapes_patch() -> str:
    """Two hunks in one file, plus one very long added line."""
    long_line = "func wide() string { return \"" + "x" * 400 + "\" }"
    return "\n".join(
        [
            "*** Begin Patch",
            "*** Update File: shapes.go",
            "@@",
            " func shape01() {}",
            "-func shape02() {}",
            "+func shape02() { changed() }",
            "+" + long_line,
            "@@",
            " func shape08() {}",
            "-func shape09() {}",
            "+func shape09() { changed() }",
            "*** End Patch",
        ]
    )


def scenario_shapes(report: Report, provider: str, mock):
    """G: a modified file with two hunks, removals, a separator and a wide line."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 60)
    try:
        ask(pty_, "create shapes.go then patch it")
        pty_.wait_for("shape02", 180)
        pty_.wait_stable(1.0, 60)
        report.dump("G-two-hunks-folded", pty_)
        # Both edit nodes start expanded; fold the earlier one is not needed —
        # scroll to the patched node's head and read its rows.
        found = scroll_until(pty_, "shape09", up=False, tries=10)
        view = pty_.screen.text()
        report.dump("G-two-hunks", pty_)
        report.check("G", "the patched row is painted", found or "shape09" in view, view[-500:])
        report.check(
            "G",
            "the update paints real removals and additions",
            any("│ -" in ln for ln in pty_.screen.lines())
            and any("│ +" in ln for ln in pty_.screen.lines()),
            view[-800:],
        )
        report.check(
            "G",
            "the two hunks are separated by an unchanged-lines marker",
            "⋯" in view,
            view[-800:],
        )
        report.check(
            "G",
            "no elision hint: every hunk row is painted",
            "还有" not in view and "lines hidden" not in view,
        )
        # The 400-character line soft-wraps and stays reachable.
        wrapped = [ln for ln in pty_.screen.lines() if "xxx" in ln]
        report.check("G", "the very long added line is painted (wrapped)", bool(wrapped))
        # A 24-column frame must keep the same rows.
        pty_.resize(24, 40)
        pty_.wait_stable(1.0, 30)
        report.dump("G-24x40", pty_)
        overflow = [ln for ln in pty_.screen.lines() if len(ln) > 24]
        report.check("G", "the narrow frame does not overflow", not overflow, str(overflow[:3]))
        return pty_
    finally:
        pty_.close()


def scenario_live_provider(report: Report, provider: str, mock):
    """Live-model closure: a real provider turn really writes a file, and the
    applied diff is painted in full and foldable in a real PTY."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 130)
    try:
        pty_.write(
            "在当前目录创建 probe_real.go，正好 103 行，每行是 `// line N`，N 从 1 到 103。"
            "只创建这一个文件，不要运行命令。"
        )
        time.sleep(0.3)
        pty_.key("\r")
        ok = pty_.wait_for("probe_real.go", 300)
        pty_.wait_stable(2.0, 300)
        report.dump("LIVE-live-provider", pty_)
        report.check("LIVE", "the live model produced an edit for the requested file", ok, pty_.screen.text()[:600])
        rows = diff_rows(pty_)
        report.check(
            "LIVE",
            "the applied diff is painted in full (no elision hint)",
            rows and "还有" not in pty_.screen.text() and "lines hidden" not in pty_.screen.text(),
            pty_.screen.text()[-600:],
        )
        # Fold, read the reported total, and prove the painted row count equals it.
        head = find_edit_head(pty_, "probe_real.go")
        report.check("LIVE", "the edit head row is on screen", head is not None)
        if head is None:
            return pty_
        painted = len(rows)
        pty_.click(head[0], 8)
        folded = pty_.screen.text()
        report.dump("LIVE-live-provider-folded", pty_)
        match = re.search(r"\+(\d+)/-(\d+)", folded)
        report.check("LIVE", "clicking the head folds to a +A/-R summary", match is not None, folded[:800])
        if match:
            added = int(match.group(1))
            report.check(
                "LIVE",
                "the summary total matches the rows actually painted (%d)" % added,
                added == painted,
                "summary +%d vs painted %d" % (added, painted),
            )
        pty_.click(head[0], 8)
        pty_.wait_stable(1.0, 30)
        report.dump("LIVE-live-provider-reopened", pty_)
        report.check(
            "LIVE",
            "a second click restores the applied diff",
            len(diff_rows(pty_)) > 0 and re.search(r"\+\d+/-\d+", pty_.screen.text()) is None,
            pty_.screen.text()[:600],
        )
        # The complete patch of the model's own edit, through the real
        # clipboard route.
        saved = clipboard_read()
        clipboard_write("")
        pty_.write("/copy diff")
        pty_.key("\r")
        pty_.wait_stable(1.2, 60)
        report.dump("LIVE-after-copy-diff", pty_)
        patch = clipboard_read()
        if saved is not None:
            clipboard_write(saved)
        report.check(
            "LIVE",
            "/copy diff yields a unified diff of the model's own edit",
            patch is not None
            and patch.startswith("--- ")
            and "probe_real.go" in patch
            and "@@" in patch
            and len(patch) > 200,
            repr((patch or "")[:200]),
        )
        return pty_
    finally:
        pty_.close()


def wait_until(pty_: Pty, predicate, timeout: float = 300.0) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        if predicate(pty_.screen.text()):
            return True
        if not pty_.alive():
            return predicate(pty_.screen.text())
        time.sleep(0.2)
    return predicate(pty_.screen.text())


def scenario_goal_pause_badge(report: Report, provider: str, mock):
    """A Chat-committed goal keeps its OWN pause while an ordinary question is
    answered, and loses it only when the user authorizes continuing."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 44)
    try:
        pty_.write(
            "请先用 commit_task 登记这件事，然后完成两个阶段："
            "阶段 1 在仓库根目录创建 stage-1.txt（内容为一行 `stage 1`），"
            "然后运行 sh -c 'cat stage-1.txt' 确认；阶段 1 完成后停下，等待我确认再执行阶段 2；"
            "阶段 2 创建 stage-2.txt（内容为一行 `stage 2`），"
            "然后运行 sh -c 'cat stage-2.txt' 确认。开始前先用 update_plan 记录这两个阶段。"
        )
        pty_.key("\r")
        paused = pty_.wait_for("已暂停", 900)
        pty_.wait_stable(2.0, 120)
        report.dump("BADGE-paused", pty_)
        screen = pty_.screen.text()
        report.check(
            "BADGE",
            "a Chat-committed goal paused by the user says so in the header",
            paused and "已暂停" in screen,
            screen[:800],
        )
        report.check(
            "BADGE",
            "the plan strip names the user's confirmation",
            "等待用户确认" in screen,
            screen[:800],
        )
        report.check(
            "BADGE",
            "stage 2 is not written before the user authorizes it",
            not os.path.exists(os.path.join(env.repo, "stage-2.txt")),
        )

        ask(pty_, "刚才阶段 1 修改了哪些文件？测试通过了吗？")
        pty_.wait_stable(2.0, 300)
        report.dump("BADGE-after-question", pty_)
        screen = pty_.screen.text()
        report.check(
            "BADGE",
            "an ordinary question keeps the paused goal on screen",
            "已暂停" in screen,
            screen[:900],
        )
        report.check(
            "BADGE",
            "the question never runs the withheld stage",
            not os.path.exists(os.path.join(env.repo, "stage-2.txt")),
        )

        ask(pty_, "确认，继续阶段 2，把剩余任务完成并验收。")
        report.check(
            "BADGE",
            "authorizing the continuation removes the pause badge",
            wait_until(pty_, lambda text: "已暂停" not in text, 900),
            pty_.screen.text()[:900],
        )
        completed = wait_until(pty_, lambda text: "已完成" in text, 600)
        pty_.wait_stable(2.0, 120)
        report.dump("BADGE-completed", pty_)
        report.check(
            "BADGE",
            "the same goal reports completion only after the real delivery",
            completed and os.path.exists(os.path.join(env.repo, "stage-2.txt")),
            pty_.screen.text()[:900],
        )
        return pty_
    finally:
        pty_.close()


def goal_rows(home: str):
    """The durable goal rows the runtime wrote, read from its own store."""
    import glob
    import sqlite3

    paths = glob.glob(os.path.join(home, "**", "*.db"), recursive=True)
    paths += glob.glob(os.path.join(home, "**", "*.sqlite*"), recursive=True)
    rows = []
    for path in paths:
        try:
            conn = sqlite3.connect("file:%s?mode=ro" % path, uri=True)
        except sqlite3.Error:
            continue
        try:
            cur = conn.execute(
                "SELECT id, objective, state, amendments, user_paused_at FROM goals"
            )
        except sqlite3.Error:
            conn.close()
            continue
        for row in cur.fetchall():
            rows.append(
                {
                    "id": row[0],
                    "objective": row[1],
                    "state": row[2],
                    "amendments": row[3],
                    "user_paused_at": row[4],
                }
            )
        conn.close()
    return rows


def scenario_goal_chat_amend(report: Report, provider: str, mock):
    """A supplement typed to a paused goal with no `继续` is durable scope: the
    pause stays, the withheld stage stays unexecuted, and only the user's own
    authorization continues the same goal."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 44)
    try:
        pty_.write(
            "请先用 commit_task 登记这件事，然后完成两个阶段："
            "阶段 1 在仓库根目录创建 stage-1.txt（内容为一行 `stage 1`），"
            "然后运行 sh -c 'cat stage-1.txt' 确认；阶段 1 完成后停下，等待我确认再执行阶段 2；"
            "阶段 2 创建 stage-2.txt（内容为一行 `stage 2`），"
            "然后运行 sh -c 'cat stage-2.txt' 确认。开始前先用 update_plan 记录这两个阶段。"
        )
        pty_.key("\r")
        paused = pty_.wait_for("已暂停", 900)
        pty_.wait_stable(2.0, 120)
        report.dump("AMEND-paused", pty_)
        report.check(
            "AMEND",
            "the goal pauses at the user's own boundary",
            paused and "已暂停" in pty_.screen.text(),
            pty_.screen.text()[:800],
        )
        before = goal_rows(env.home)
        report.check("AMEND", "one durable goal exists", len(before) == 1, repr(before))
        goal_id = before[0]["id"] if before else ""

        # The supplement: a real new deliverable, and no `继续` anywhere.
        ask(
            pty_,
            "顺便把 stage-extra.txt 也补上，内容为一行 `extra`，"
            "然后运行 sh -c 'cat stage-extra.txt' 确认。阶段 2 仍然等我确认。",
        )
        pty_.wait_stable(2.0, 120)
        report.dump("AMEND-supplement", pty_)
        screen = pty_.screen.text()
        after = goal_rows(env.home)
        report.check(
            "AMEND",
            "the supplement is durable scope without a resume keyword",
            any("stage-extra.txt" in (row["amendments"] or "") for row in after),
            repr(after),
        )
        report.check(
            "AMEND",
            "adding a requirement does not release the pause",
            bool(after) and after[0]["user_paused_at"] is not None
            and "已暂停" in screen,
            repr(after) + "\n" + screen[:600],
        )
        report.check(
            "AMEND",
            "the goal keeps its identity",
            bool(after) and after[0]["id"] == goal_id,
            repr(after),
        )
        report.check(
            "AMEND",
            "the withheld stage is still not executed",
            not os.path.exists(os.path.join(env.repo, "stage-2.txt")),
        )

        ask(pty_, "确认，继续阶段 2，并把 stage-extra.txt 一起做完和验收。")
        done = wait_until(
            pty_, lambda text: "已完成" in text and "已暂停" not in text, 900
        )
        pty_.wait_stable(2.0, 120)
        report.dump("AMEND-completed", pty_)
        final = goal_rows(env.home)
        report.check(
            "AMEND",
            "the user's authorization continues the same goal to completion",
            done and os.path.exists(os.path.join(env.repo, "stage-2.txt")),
            pty_.screen.text()[:800],
        )
        report.check(
            "AMEND",
            "the supplement survives the continuation",
            any("stage-extra.txt" in (row["amendments"] or "") for row in final),
            repr(final),
        )
        return pty_
    finally:
        pty_.close()


def scenario_goal_multi_goal_live(report: Report, provider: str, mock):
    """Goal A stays owed and paused while goal B executes; the live TUI shows B,
    not A's wait, and A is still intact afterwards."""
    env = make_env(provider, mock)
    pty_ = boot(report, env, 120, 44)
    try:
        pty_.write(
            "请先用 commit_task 登记这件事，然后完成两个阶段："
            "阶段 1 在仓库根目录创建 stage-1.txt（内容为一行 `stage 1`），"
            "然后运行 sh -c 'cat stage-1.txt' 确认；阶段 1 完成后停下，等待我确认再执行阶段 2；"
            "阶段 2 创建 stage-2.txt（内容为一行 `stage 2`），"
            "然后运行 sh -c 'cat stage-2.txt' 确认。开始前先用 update_plan 记录这两个阶段。"
        )
        pty_.key("\r")
        paused = pty_.wait_for("已暂停", 900)
        pty_.wait_stable(2.0, 120)
        report.dump("MULTIGOAL-a-paused", pty_)
        report.check(
            "MULTIGOAL",
            "goal A pauses at the user's boundary and stays owed",
            paused and "已暂停" in pty_.screen.text(),
            pty_.screen.text()[:800],
        )
        goals_before = goal_rows(env.home)
        goal_a = goals_before[0]["id"] if goals_before else ""
        report.check("MULTIGOAL", "goal A is the only goal", len(goals_before) == 1, repr(goals_before))
        report.check(
            "MULTIGOAL",
            "goal A's pause is a durable row, not only a badge",
            bool(goals_before) and goals_before[0]["user_paused_at"] is not None,
            repr(goals_before),
        )

        # Goal B starts while A is still owed and paused.
        pty_.write(
            "/goal 请先用 commit_task 登记，然后一次完成，中间不要停下来等我确认："
            "在仓库根目录创建 export-b1.txt（内容一行 `b1`），"
            "然后运行 sh -c 'cat export-b1.txt' 确认。"
        )
        time.sleep(0.2)
        pty_.key("\r")
        # Sample WHILE B executes: the wait badge of A may not be on screen.
        live_frames = []
        deadline = time.time() + 300
        while time.time() < deadline:
            text = pty_.screen.text()
            if "export-b1.txt" in text:
                live_frames.append(text)
                if len(live_frames) >= 3:
                    break
            if not pty_.alive():
                break
            time.sleep(0.2)
        report.dump("MULTIGOAL-b-live", pty_)
        live = pty_.screen.text()
        report.check(
            "MULTIGOAL",
            "goal B is on screen WHILE it executes",
            len(live_frames) > 0 and "export-b1.txt" in live,
            live[:900],
        )
        report.check(
            "MULTIGOAL",
            "goal B's window does not show goal A's pause",
            bool(live_frames) and "已暂停" not in live,
            live[:400],
        )
        goals_live = goal_rows(env.home)
        report.check(
            "MULTIGOAL",
            "both goals are durable, A still paused, B without one",
            len(goals_live) == 2
            and any(row["id"] == goal_a and row["user_paused_at"] is not None for row in goals_live)
            and all(row["user_paused_at"] is None for row in goals_live if row["id"] != goal_a),
            repr(goals_live),
        )

        wait_until(pty_, lambda text: "已完成" in text, 900)
        pty_.wait_stable(2.0, 120)
        report.dump("MULTIGOAL-b-settled", pty_)
        final = goal_rows(env.home)
        report.check(
            "MULTIGOAL",
            "goal A's history is intact after goal B",
            any(row["id"] == goal_a and row["user_paused_at"] is not None for row in final),
            repr(final),
        )
        report.check(
            "MULTIGOAL",
            "goal B never executed goal A's withheld stage",
            not os.path.exists(os.path.join(env.repo, "stage-2.txt")),
            pty_.screen.text()[:600],
        )
        print(
            "  goal B delivered export-b1.txt: %s"
            % os.path.exists(os.path.join(env.repo, "export-b1.txt"))
        )
        return pty_
    finally:
        pty_.close()


def repo_processes(repo: str, token: str, binary: str | None = None) -> list[int]:
    """PIDs of `leveler <token>` processes owning this isolated repo.

    `ps` is the only view that sees a process detached from the TUI's session;
    the argv is matched by the real executable, the subcommand token and the
    repo it serves, never by a substring any shell in this process tree could
    also contain. A scenario that runs the client from its own copy passes
    that path so the copy's processes are found too.
    """
    binary = os.path.realpath(binary or BIN)
    wanted = {repo, os.path.realpath(repo)}
    out = subprocess.run(
        ["ps", "-axo", "pid=,command="], capture_output=True, text=True
    ).stdout
    pids = []
    for line in out.splitlines():
        pid_text, _, command = line.strip().partition(" ")
        if not pid_text.isdigit():
            continue
        words = command.split()
        if not words or os.path.realpath(words[0]) != binary:
            continue
        if token not in words:
            continue
        if not any(path in command for path in wanted):
            continue
        pids.append(int(pid_text))
    return pids


def daemon_pids(repo: str, binary: str | None = None) -> list[int]:
    return repo_processes(repo, "serve", binary)


def cli_json(env: "Env", args: list[str]):
    out = subprocess.run(
        [BIN, "--repo", env.repo, *args],
        capture_output=True,
        text=True,
        env=env.env,
        timeout=120,
    )
    if out.returncode != 0:
        raise RuntimeError(out.stderr.strip() or out.stdout.strip())
    return json.loads(out.stdout)


def wait_file(path: str, needle: str, timeout: float) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        try:
            with open(path) as fh:
                if needle in fh.read():
                    return True
        except FileNotFoundError:
            pass
        time.sleep(0.1)
    return False


def composer_text(pty_: Pty) -> str:
    """The composer box's rows, located from its own top border.

    A transcript row that happens to sit near the bottom is not the input: a
    refused submission is put back into THIS box, so only the box can tell
    whether the text is still waiting to be sent.
    """
    lines = pty_.screen.lines()
    for i in range(len(lines) - 1, -1, -1):
        if "╭" in lines[i]:
            return "\n".join(lines[i:])
    return "\n".join(lines[-3:])


def scenario_reconnect(report: Report, provider: str, mock):
    """A genuinely killed runtime: an idle restart must invent no lost work,
    a restart during a live turn must say that turn was interrupted, and the
    replacement must re-read the session and its background work without
    re-running or wrongly cancelling what the model started."""
    env = Env(provider, mock, short=True)
    pty_ = boot(report, env, 120, 44)
    ran = os.path.join(env.repo, "ran.txt")
    task_id = None
    try:
        first = daemon_pids(env.repo)
        report.check(
            "reconnect",
            "the TUI attached to a detached runtime for this repo",
            bool(first),
            repr(first),
        )
        if not first:
            return pty_
        report.check(
            "reconnect",
            "a first connection reports no interruption",
            "刚才的任务已中断" not in pty_.screen.text()
            and "刚才的任务状态无法确认" not in pty_.screen.text(),
            pty_.screen.text()[:500],
        )

        # 1. A real model turn starts a real background task owned by the
        #    independent execution host: it must outlive the runtime it was
        #    started from.
        pty_.write(
            "调用 run_command 工具，只调用一次：program=\"sh\"，"
            "args=[\"-c\",\"sleep 600\"]，background=true，"
            "background_lifetime=\"runtime\"。这是后台任务，不要等待它结束，"
            "也不要调用其他工具。"
        )
        pty_.key("\r")
        started = wait_until(pty_, lambda text: "后台 1" in text, timeout=300)
        report.dump("RECONNECT-1-background-started", pty_)
        report.check(
            "reconnect",
            "a real model turn started a real background task",
            started,
            pty_.screen.text()[-700:],
        )
        if not started:
            return pty_
        inventory = cli_json(env, ["background", "list", "--json"])
        hosted = [
            task
            for task in inventory
            if task.get("status") == "running" and task.get("owner") == "execution-host"
        ]
        report.check(
            "reconnect",
            "the running task is owned by the execution host",
            bool(hosted),
            json.dumps(inventory, ensure_ascii=False)[:800],
        )
        if not hosted:
            return pty_
        task_id = hosted[0]["task_id"]

        # Let the model's turn finish before the fault: the restart must be
        # judged on a settled session with nothing in flight but the host-owned
        # service.
        idle = wait_until(
            pty_,
            lambda text: "Esc 打断" not in text and "等待模型" not in text,
            timeout=300,
        )
        report.check(
            "reconnect",
            "the model turn before the restart settled",
            idle,
            pty_.screen.text()[-700:],
        )
        if not idle:
            return pty_

        before = daemon_pids(env.repo)
        for pid in before:
            os.kill(pid, signal.SIGKILL)
        after: list[int] = []
        deadline = time.time() + 60
        while time.time() < deadline and not after:
            after = [pid for pid in daemon_pids(env.repo) if pid not in before]
            time.sleep(0.2)
        report.dump("RECONNECT-2-restarted", pty_)
        report.check(
            "reconnect",
            "a replacement runtime is alive with a different pid",
            bool(after),
            "before %r after %r" % (before, after),
        )
        # The turn was settled before the kill: only a host-owned background
        # service was running, so nothing this client executed was at risk.
        # A reconnected view must not invent lost work; the snapshot that
        # follows is what re-adopts the surviving task (checked below).
        time.sleep(3.0)
        report.check(
            "reconnect",
            "an idle restart invents no lost work",
            "刚才的任务已中断" not in pty_.screen.text()
            and "刚才的任务状态无法确认" not in pty_.screen.text(),
            pty_.screen.text()[-700:],
        )

        # 2. The runtime is authoritative again: the host-owned task is still
        #    running and the TUI re-adopts it from the snapshot.
        inventory = cli_json(env, ["background", "list", "--json"])
        still = [
            task
            for task in inventory
            if task.get("task_id") == task_id and task.get("status") == "running"
        ]
        report.check(
            "reconnect",
            "the background task survived the runtime restart",
            bool(still),
            json.dumps(inventory, ensure_ascii=False)[:800],
        )
        report.check(
            "reconnect",
            "the TUI still shows the re-verified background task",
            wait_until(pty_, lambda text: "后台 1" in text, timeout=30),
            pty_.screen.text()[-700:],
        )

        # The crashed turn had no tool call in flight, so the runtime has no
        # crash window to acknowledge. A new turn must reach the real model:
        # reconnect restores command authority, not just the picture.
        marker = "R3C0NN3CT-OK"
        pty_.write("只回复这个标记，不要任何其他内容：%s" % marker)
        pty_.key("\r")

        def answered() -> bool:
            # The echo of the prompt is not an answer: the composer must be
            # empty (a refused submission is put back into it), and the marker
            # must be in the transcript as both the user row and the reply.
            return (
                pty_.screen.text().count(marker) >= 2
                and marker not in composer_text(pty_)
            )

        answered_ok = wait_until(pty_, lambda _: answered(), timeout=300)
        report.dump("RECONNECT-3-answered", pty_)
        report.check(
            "reconnect",
            "a new turn after the restart reaches the model and answers",
            answered_ok,
            pty_.screen.text()[-700:],
        )

        # 3. Kill the runtime in the middle of a foreground tool call. The
        #    command is a child in its own process group and finishes on its
        #    own; the new runtime must not run it a second time.
        pty_.write(
            "调用 shell_command 工具，只调用一次，命令为："
            "echo start >> %s; sleep 8; echo end >> %s"
            % (ran, ran)
            + "。这是前台命令，不要设置 background，不要等待别的任务，"
            "也不要调用其他工具。"
        )
        pty_.key("\r")
        began = wait_file(ran, "start", 300)
        report.dump("RECONNECT-4-command-running", pty_)
        report.check(
            "reconnect",
            "the real model started the command whose work must not repeat",
            began,
            pty_.screen.text()[-700:],
        )
        if not began:
            return pty_
        before = daemon_pids(env.repo)
        for pid in before:
            os.kill(pid, signal.SIGKILL)
        restarted = wait_until(
            pty_, lambda text: "刚才的任务已中断" in text, timeout=90
        )
        report.dump("RECONNECT-5-interrupted", pty_)
        report.check(
            "reconnect",
            "a restart during a tool call is reported",
            restarted,
            pty_.screen.text()[-700:],
        )
        # The orphaned command completes exactly once; then a quiet window
        # proves the replacement runtime did not resume or re-run the turn.
        finished = wait_file(ran, "end", 30)
        time.sleep(5.0)
        with open(ran) as fh:
            lines = fh.read().splitlines()
        report.check(
            "reconnect",
            "the interrupted command finished on its own",
            finished,
            repr(lines),
        )
        report.check(
            "reconnect",
            "the reconnect did not execute the command a second time",
            lines == ["start", "end"],
            repr(lines),
        )
        report.check(
            "reconnect",
            "the interrupted turn is not left busy in the TUI",
            "Esc 打断" not in pty_.screen.text(),
            pty_.screen.text()[-700:],
        )
        report.check(
            "reconnect",
            "the unresolved interrupted call is surfaced, not hidden",
            "状态未知" in pty_.screen.text(),
            pty_.screen.text()[-700:],
        )

        # The unresolved tool call is a real safety gate: the runtime must
        # refuse to guess whether it ran, keep the user's next input, and say
        # exactly how to acknowledge it. Nothing may be replayed silently.
        pty_.write("只回复这个标记，不要任何其他内容：%s" % marker)
        pty_.key("\r")
        gated = wait_until(
            pty_, lambda text: "任务恢复已暂停" in text, timeout=120
        )
        report.dump("RECONNECT-6-recovery-required", pty_)
        with open(ran) as fh:
            after_gate = fh.read().splitlines()
        report.check(
            "reconnect",
            "an interrupted call stops the next turn instead of guessing",
            gated,
            pty_.screen.text()[-700:],
        )
        # A refused turn opens the recovery acknowledgement over the draft.
        # Backing out of it acknowledges nothing and must leave the refused
        # input where the user can submit it once the gate is resolved.
        overlay_open = "恢复确认" in pty_.screen.text()
        report.check(
            "reconnect",
            "the refusal presents the acknowledgement without acting",
            overlay_open,
            pty_.screen.text()[-500:],
        )
        if overlay_open:
            pty_.key("\x1b")  # cancel: nothing is acknowledged
            time.sleep(0.3)
        report.check(
            "reconnect",
            "the refused input is preserved for the user",
            marker in composer_text(pty_),
            pty_.screen.text()[-400:],
        )
        report.check(
            "reconnect",
            "the refused turn executed nothing",
            after_gate == ["start", "end"],
            repr(after_gate),
        )

        # 5. Warnings this incident produced are transient, not permanent chrome.
        cleared = wait_until(
            pty_,
            lambda text: "刚才的任务已中断" not in text
            and "刚才的任务状态无法确认" not in text,
            timeout=30,
        )
        report.dump("RECONNECT-7-notice-cleared", pty_)
        report.check(
            "reconnect",
            "the resolved reconnect notice does not stay on screen",
            cleared,
            pty_.screen.text()[-500:],
        )
        return pty_
    finally:
        # The isolated runtime and its host-owned service are test processes:
        # stop the service through its own owner first (the host outlives the
        # runtime), then retire the detached owners and the TUI.
        if task_id is not None:
            try:
                subprocess.run(
                    [BIN, "--repo", env.repo, "background", "stop", task_id],
                    capture_output=True,
                    text=True,
                    env=env.env,
                    timeout=60,
                )
            except Exception:
                pass
        for token in ("serve", "execution-host"):
            try:
                for pid in repo_processes(env.repo, token):
                    os.kill(pid, signal.SIGKILL)
            except Exception:
                pass
        pty_.close()


def scenario_version_mismatch(report: Report, provider: str, mock):
    """A running client whose own binary was replaced must not mistake the
    replacement runtime for its own generation: it says the versions do not
    match, never claims an update finished, and keeps the fault visible after
    the transient warning expires.

    Two REAL builds are needed. `LEVELER_ALT_BIN` must point at a binary of
    this source whose build fingerprint differs; the scenario proves the
    fingerprint differs before the fault, so a same-file copy cannot pass.
    """
    if TRANSPORT != "daemon":
        report.check(
            "version-mismatch",
            "the fault needs a detached runtime",
            False,
            "run with --transport daemon",
        )
        return None
    alt = os.environ.get("LEVELER_ALT_BIN", "")
    if not alt or not os.path.isfile(alt):
        report.check(
            "version-mismatch",
            "a second build is available (LEVELER_ALT_BIN)",
            False,
            "set LEVELER_ALT_BIN to a binary built from a different fingerprint",
        )
        return None
    mine, other = fingerprint_of(BIN), fingerprint_of(alt)
    report.check(
        "version-mismatch",
        "the two binaries are different build identities",
        bool(mine) and bool(other) and mine != other,
        "app %s · alt %s" % (mine or "<none>", other or "<none>"),
    )
    if not mine or not other or mine == other:
        return None
    env = Env(provider, mock, short=True)
    launcher = os.path.join(env.root, "leveler")
    shutil.copy2(BIN, launcher)
    os.chmod(launcher, 0o755)
    pty_ = boot(report, env, 120, 44, bin_path=launcher)
    try:
        first = daemon_pids(env.repo, launcher)
        report.check(
            "version-mismatch",
            "the TUI attached to a runtime started from its own path",
            bool(first),
            repr(first),
        )
        if not first:
            return pty_

        # Replace the file the running client re-execs, atomically: an
        # in-place write would fault the running process's mapped image,
        # while a rename swaps the directory entry under the same path.
        staged = launcher + ".new"
        shutil.copy2(alt, staged)
        os.chmod(staged, 0o755)
        os.replace(staged, launcher)
        for pid in first:
            os.kill(pid, signal.SIGKILL)

        # Capture the replacement process while waiting for the notice. The
        # client may retire and respawn it during the retry loop, so a pid
        # sampled only after the text appears can be gone; what must be proven
        # is that a replacement runtime WAS alive at the time of the fault,
        # not that it survives the whole scenario.
        after: list[int] = []
        mismatch = False
        deadline = time.time() + 120
        while time.time() < deadline:
            observed = [
                pid for pid in daemon_pids(env.repo, launcher) if pid not in first
            ]
            if observed:
                after = observed
            if "CodeLeveler 版本不一致" in pty_.screen.text():
                mismatch = True
                break
            time.sleep(0.2)
        report.dump("VERSION-MISMATCH-1-warning", pty_)
        if not mismatch:
            # The client never attaches to the replacement runtime, so the
            # event that carries its build never reaches the TUI. Record the
            # observed gate and stop: the remaining assertions depend on an
            # attached stream and cannot be proven here.
            log_path = os.path.join(env.home, "leveler.log")
            try:
                with open(log_path) as fh:
                    log_tail = "".join(fh.readlines()[-60:])
            except OSError:
                log_tail = ""
            with open(
                os.path.join(EVIDENCE, "VERSION-MISMATCH-host-guard.log"), "w"
            ) as fh:
                fh.write(log_tail)
            guard = [
                line.strip()
                for line in log_tail.splitlines()
                if "runtime was not replaced correctly" in line
            ]
            report.block(
                "version-mismatch",
                "the version-mismatch notice is reachable in the real client path",
                "no subscription was ever attached to the replacement runtime, so no "
                "build observation exists to render; the runtime host refused it: "
                + (guard[-1][-240:] if guard else "<no guard line in daemon log>"),
            )
            return pty_
        report.check(
            "version-mismatch",
            "a runtime restarted from a different build is named as a version mismatch",
            mismatch,
            pty_.screen.text()[-700:],
        )
        report.check(
            "version-mismatch",
            "a version mismatch never claims an update was installed",
            "更新已安装" not in pty_.screen.text(),
            pty_.screen.text()[-500:],
        )

        report.check(
            "version-mismatch",
            "a replacement runtime with a different pid was alive during the fault",
            bool(after),
            "before %r observed during the fault %r" % (first, after),
        )

        # The warning is transient by design; the fault is not. After its TTL
        # the state strip must still report an open issue.
        time.sleep(2.0)
        warning_gone = wait_until(
            pty_,
            lambda text: "CodeLeveler 版本不一致" not in text,
            timeout=30,
        )
        report.dump("VERSION-MISMATCH-2-persistent", pty_)
        report.check(
            "version-mismatch",
            "the transient warning expires",
            mismatch and warning_gone,
            pty_.screen.text()[-400:],
        )
        report.check(
            "version-mismatch",
            "the unresolved mismatch stays visible as an open issue",
            "问题 1" in pty_.screen.text(),
            pty_.screen.text()[-400:],
        )

        # The issue page names the fault and both identities; it is opened by
        # the same click a user makes on the strip's issue cell.
        spot = pty_.screen.find("问题")
        if spot is not None:
            row, char_col = spot
            line = pty_.screen.lines()[row]
            col = sum(cell_width(ch) for ch in line[:char_col])
            pty_.click(row, col)
            pty_.wait_stable(0.6, 20)
            # The list stays compact; the diagnostic line (both build
            # identities) is the selected row's expanded body.
            pty_.key("\r")
            pty_.wait_stable(0.6, 20)
        report.dump("VERSION-MISMATCH-3-issue-page", pty_)
        page = pty_.screen.text()
        report.check(
            "version-mismatch",
            "the issue page names the mismatch and both build identities",
            "CodeLeveler 版本不一致" in page
            and "当前应用" in page
            and "后台" in page,
            page[-700:],
        )
        return pty_
    finally:
        for token in ("serve", "execution-host"):
            try:
                for pid in repo_processes(env.repo, token, launcher):
                    os.kill(pid, signal.SIGKILL)
            except Exception:
                pass
        pty_.close()


def fingerprint_of(path: str) -> str:
    """The build fingerprint a binary carries, read from its own bytes.

    The value is compiled in by `leveler-core/build.rs`; extracting it from
    the artifact is what proves two files are really different identities
    rather than one file copied twice.
    """
    out = subprocess.run(
        ["grep", "-aoE", r"[0-9a-f]{40}-[0-9a-f]{16}", path],
        capture_output=True,
        text=True,
    ).stdout.split()
    return out[0] if out else ""


SCENARIOS = {
    "display": (
        scenario_default_display,
        lambda: [tool_turn("write_file", create_file_args()), text_turn("已创建 cookies_test.go，共 103 行。")],
    ),
    "mouse": (
        scenario_mouse,
        lambda: [
            tool_turn("write_file", create_file_args()),
            tool_turn("write_file", json.dumps({"path": "second.go", "content": "package second\n\nvar secondLine3 = 3\n"})),
            text_turn("已创建两个文件。"),
        ],
    ),
    "keyboard": (
        scenario_keyboard,
        lambda: [
            tool_turn("run_command", json.dumps({"program": "echo", "args": ["hi"]})),
            tool_turn("write_file", create_file_args()),
            text_turn("完成。"),
        ],
    ),
    "history": (
        scenario_history,
        lambda: [tool_turn("write_file", create_file_args()), text_turn("已创建 cookies_test.go，共 103 行。")],
    ),
    "copy": (
        scenario_copy,
        lambda: [
            tool_turn("write_file", json.dumps({"path": "cookies_test.go", "content": mixed_content()})),
            text_turn("已创建 cookies_test.go，共 103 行。"),
        ],
    ),
    "shapes": (
        scenario_shapes,
        lambda: [
            tool_turn("write_file", json.dumps({"path": "shapes.go", "content": shapes_content()})),
            tool_turn("apply_patch", json.dumps({"patch": shapes_patch()})),
            text_turn("已创建并修改 shapes.go。"),
        ],
    ),
    "live": (scenario_live_provider, lambda: []),
    "badge": (scenario_goal_pause_badge, lambda: []),
    "amend": (scenario_goal_chat_amend, lambda: []),
    "multigoal": (scenario_goal_multi_goal_live, lambda: []),
    "reconnect": (scenario_reconnect, lambda: []),
    "version-mismatch": (scenario_version_mismatch, lambda: []),
    "sizes": (
        scenario_sizes,
        lambda: [tool_turn("write_file", create_file_args()), text_turn("已创建 cookies_test.go，共 103 行。")],
    ),
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", choices=["mock", "real"], default="mock")
    parser.add_argument("--transport", choices=["in-process", "daemon"], default="in-process")
    parser.add_argument("--only", default="", help="comma-separated scenario names")
    args = parser.parse_args()
    global TRANSPORT
    TRANSPORT = args.transport
    os.makedirs(EVIDENCE, exist_ok=True)
    if not os.path.exists(BIN):
        print("missing %s — build it first (cargo build -p leveler-cli)" % BIN)
        return 2
    selected = [s for s in (args.only.split(",") if args.only else SCENARIOS) if s]
    if args.provider == "mock":
        # The live-provider scenario asserts on a model's own edit; scripted
        # frames cannot stand in for it, so it is not run rather than faked.
        skipped = [s for s in selected if s in ("live", "badge", "amend", "multigoal", "reconnect")]
        for name in skipped:
            print("=== scenario %s SKIPPED (needs --provider real) ===" % name)
        selected = [s for s in selected if s not in ("live", "badge", "amend", "multigoal", "reconnect")]
    report = Report(args.provider)
    for name in selected:
        fn, script = SCENARIOS[name]
        mock = None
        if args.provider == "mock":
            mock = MockProvider()
            mock.turns = script()
        print("=== scenario %s (%s provider) ===" % (name, args.provider))
        try:
            fn(report, args.provider, mock)
        except Exception as exc:  # a crashed scenario is a FAIL, not a skip
            report.check(name, "scenario completed", False, repr(exc))
            import traceback

            traceback.print_exc()
        finally:
            if mock is not None:
                mock.stop()
    path = report.save()
    print(
        "\nassertions: %d, failures: %d, blocked: %d"
        % (len(report.results), report.failures, report.blocked)
    )
    print("evidence: %s" % path)
    if report.failures:
        return 1
    return 3 if report.blocked else 0


if __name__ == "__main__":
    sys.exit(main())
