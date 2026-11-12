#!/usr/bin/env python3
"""Real-PTY acceptance harness for the welcome mascot image.

Drives the real `leveler` binary on a real pseudo-terminal and asserts on the
bytes the terminal receives (Kitty APC / iTerm2 OSC 1337) plus the VT screen
model from pty_dogfood.py. It never imports CodeLeveler code.

Covered:
  * protocol matrix: ghostty/kitty, iTerm2, WezTerm, SSH kitty, LC_TERMINAL
  * conservative hiding: Apple Terminal, VS Code, unknown, TERM=dumb, screen,
    tmux (including Ghostty inside tmux), small viewport, NO_COLOR
  * idle frames do not re-upload; typing does not lose input
  * resize replaces the Kitty id and deletes the old one
  * first real message retires the image; Ctrl+C exit deletes the last id
  * resume does not paint or upload the welcome image

Evidence: target/welcome-image-dogfood/{screens,raw,report.json}.
"""
from __future__ import annotations

import argparse
import base64
import glob
import os
import re
import signal
import sqlite3
import struct
import subprocess
import sys
import time
import zlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_dogfood import MockProvider, Report, isolated_env, spawn, text_turn  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_BIN = os.path.join(ROOT, "target", "debug", "leveler")
OUT = os.path.join(ROOT, "target", "welcome-image-dogfood")
PLACEHOLDER = "\U0010EEEE".encode()
APC = re.compile(rb"\x1b_G(.*?)\x1b\\", re.S)

CLEARED = [
    "TERM_PROGRAM",
    "LC_TERMINAL",
    "TMUX",
    "STY",
    "SSH_CONNECTION",
    "NO_COLOR",
    "WT_SESSION",
]


def ops(raw: bytes) -> list[tuple[str, int]]:
    """Kitty transmissions and deletes, in wire order."""
    found = []
    for match in APC.finditer(raw):
        control = match.group(1).split(b";", 1)[0]
        ident = re.search(rb"(?:^|,)i=(\d+)", control)
        if ident is None:
            continue
        if b"a=T" in control:
            found.append(("tx", int(ident.group(1))))
        elif b"a=d" in control:
            found.append(("delete", int(ident.group(1))))
    return found


def tx_ids(raw: bytes) -> list[int]:
    return [value for kind, value in ops(raw) if kind == "tx"]


def deleted_ids(raw: bytes) -> list[int]:
    return [value for kind, value in ops(raw) if kind == "delete"]


def iterm_count(raw: bytes) -> int:
    return raw.count(b"\x1b]1337;File=")


def iterm_png(raw: bytes) -> bytes | None:
    match = re.search(rb"\x1b\]1337;File=[^:]*:([A-Za-z0-9+/=]+)\x07", raw)
    return base64.b64decode(match.group(1)) if match else None


def png_alpha_range(png: bytes) -> tuple[int, int, int, int] | None:
    """Decode an 8-bit non-interlaced RGBA PNG and return (w, h, min, max) alpha.

    iTerm2 composites transparent pixels over the window backdrop (a profile
    background image), not over the TUI cell colour. The product bakes the card
    canvas into the PNG, so a fully opaque payload is the observable property
    that stops the wallpaper showing through as a gray rectangle."""
    if not png.startswith(b"\x89PNG\r\n\x1a\n"):
        return None
    pos, header, idat = 8, None, bytearray()
    while pos + 8 <= len(png):
        length = struct.unpack(">I", png[pos : pos + 4])[0]
        kind = png[pos + 4 : pos + 8]
        data = png[pos + 8 : pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            header = struct.unpack(">IIBBBBB", data)
        elif kind == b"IDAT":
            idat.extend(data)
        elif kind == b"IEND":
            break
    if header is None:
        return None
    width, height, depth, color, _comp, _filter, interlace = header
    if depth != 8 or color != 6 or interlace != 0:
        return None
    try:
        pixels = zlib.decompress(bytes(idat))
    except zlib.error:
        return None
    stride = width * 4
    if len(pixels) < height * (stride + 1):
        return None
    prev = bytearray(stride)
    low, high = 255, 0
    for _ in range(height):
        kind = pixels[0]
        line = bytearray(pixels[1 : 1 + stride])
        pixels = pixels[1 + stride :]
        for i in range(stride):
            left = line[i - 4] if i >= 4 else 0
            up = prev[i]
            corner = prev[i - 4] if i >= 4 else 0
            if kind == 1:
                line[i] = (line[i] + left) & 0xFF
            elif kind == 2:
                line[i] = (line[i] + up) & 0xFF
            elif kind == 3:
                line[i] = (line[i] + (left + up) // 2) & 0xFF
            elif kind == 4:
                p = left + up - corner
                pa, pb, pc = abs(p - left), abs(p - up), abs(p - corner)
                predictor = left if pa <= pb and pa <= pc else up if pb <= pc else corner
                line[i] = (line[i] + predictor) & 0xFF
            elif kind != 0:
                return None
        for i in range(3, stride, 4):
            low = min(low, line[i])
            high = max(high, line[i])
        prev = line
    return width, height, low, high


def wait_until(predicate, timeout: float) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        if predicate():
            return True
        time.sleep(0.05)
    return predicate()


def wait_raw_stable(pty, quiet: float = 2.0, timeout: float = 30.0) -> bool:
    end = time.time() + timeout
    last_len = len(pty.raw)
    stamp = time.time()
    while time.time() < end:
        time.sleep(0.1)
        if len(pty.raw) != last_len:
            last_len = len(pty.raw)
            stamp = time.time()
        elif time.time() - stamp >= quiet:
            return True
    return False


def wait_live_image_settled(pty, quiet: float = 2.0, timeout: float = 60.0) -> bool:
    """Wait until the last transmitted Kitty id stays undeleted through a quiet
    window. Image encoding can outlast a fixed quiet period: the stream looks
    idle while the replacement is still being encoded, so a snapshot taken then
    mistakes the last *deleted* id for the live one."""
    end = time.time() + timeout
    last_len = len(pty.raw)
    stamp = time.time()
    while time.time() < end:
        time.sleep(0.1)
        raw = bytes(pty.raw)
        if len(raw) != last_len:
            last_len = len(raw)
            stamp = time.time()
            continue
        if time.time() - stamp < quiet:
            continue
        ids = tx_ids(raw)
        if ids and ids[-1] not in deleted_ids(raw):
            return True
    return False


def env_for(base: dict, **values) -> dict:
    env = dict(base)
    for key in CLEARED:
        env.pop(key, None)
    for key, value in values.items():
        if value is not None:
            env[env_key(key)] = str(value)
    return env


def env_key(name: str) -> str:
    return {
        "term": "TERM",
        "program": "TERM_PROGRAM",
        "lc": "LC_TERMINAL",
        "tmux": "TMUX",
        "ssh": "SSH_CONNECTION",
        "no_color": "NO_COLOR",
    }[name]


class Harness:
    def __init__(self, args):
        self.args = args
        self.report = Report("welcome-image")
        os.makedirs(OUT, exist_ok=True)
        os.makedirs(os.path.join(ROOT, "target", "pty-dogfood"), exist_ok=True)
        self.screens = os.path.join(OUT, "screens")
        self.raws = os.path.join(OUT, "raw")
        os.makedirs(self.screens, exist_ok=True)
        os.makedirs(self.raws, exist_ok=True)
        self.mock = MockProvider()
        self.mock.turns = [text_turn("Hello from the mock provider.")]

    def save(self, name: str, pty) -> None:
        screen_text = "\n".join(pty.screen.lines())
        with open(os.path.join(self.screens, name + ".txt"), "w") as fh:
            fh.write(screen_text + "\n")
        with open(os.path.join(self.raws, name + ".bin"), "wb") as fh:
            fh.write(bytes(pty.raw))

    def open(self, name: str, spec: dict):
        _root, home, repo, base = isolated_env("mock", self.mock, short=True)
        env = env_for(
            base,
            term=spec.get("term", "xterm-256color"),
            program=spec.get("program"),
            lc=spec.get("lc"),
            tmux=spec.get("tmux"),
            ssh=spec.get("ssh"),
            no_color=spec.get("no_color"),
        )
        cols, rows = spec.get("size", (110, 40))
        pty = spawn(env, repo, cols, rows, list(spec.get("extra", [])), bin_path=self.args.bin)
        return pty, home, repo, env

    def boot(self, name: str, pty) -> bool:
        return pty.wait_for("CodeLeveler", timeout=self.args.boot_timeout)

    def quit_and_check(self, name: str, pty, expect: str) -> None:
        # A running turn consumes the first Ctrl+C as a cancel; press again
        # after it settles, as a user would.
        for _ in range(4):
            if not pty.alive():
                break
            pty.write("\x03")
            if wait_until(lambda: not pty.alive(), 3):
                break
        exited = not pty.alive()
        self.report.check(name, "Ctrl+C exits", exited)
        if not exited:
            pty.close()
            return
        try:
            pty.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        raw = bytes(pty.raw)
        self.report.check(name, "leaves alternate screen", raw.count(b"\x1b[?1049l") >= 1)
        if expect == "kitty":
            ids = tx_ids(raw)
            last = ids[-1] if ids else None
            deletes = deleted_ids(raw)
            self.report.check(name, "exit deletes the live image id", last in deletes)
            if last in deletes and b"\x1b[?1049l" in raw:
                self.report.check(
                    name,
                    "delete precedes leaving the alternate screen",
                    raw.rfind(b"\x1b_Ga=d,d=I,i=%d" % last) < raw.rfind(b"\x1b[?1049l"),
                )

    def run_image_terminal(self, name: str, spec: dict) -> None:
        expected = spec["expect"]
        started = time.monotonic()
        pty, _home, _repo, _env = self.open(name, spec)
        try:
            if not self.boot(name, pty):
                self.report.check(name, "boots to the welcome card", False)
                return
            self.report.check(name, "boots to the welcome card", True)
            if expected == "kitty":
                arrived = wait_until(
                    lambda: PLACEHOLDER in bytes(pty.raw), self.args.image_timeout
                )
                self.report.check(
                    name,
                    "transmits a full Kitty image",
                    arrived,
                    f"{time.monotonic() - started:.2f}s spawn -> first placeholder",
                )
                raw = bytes(pty.raw)
                self.report.check(name, "uses unicode placeholders", PLACEHOLDER in raw)
                self.report.check(name, "no iTerm2 payload", iterm_count(raw) == 0)
            else:
                arrived = wait_until(
                    lambda: iterm_count(bytes(pty.raw)) > 0,
                    self.args.image_timeout,
                )
                self.report.check(
                    name,
                    "transmits an iTerm2 image",
                    arrived,
                    f"{time.monotonic() - started:.2f}s spawn -> first image payload",
                )
                raw = bytes(pty.raw)
                self.report.check(name, "no Kitty APC", b"\x1b_G" not in raw)
                png = iterm_png(raw)
                stats = png_alpha_range(png) if png else None
                self.report.check(
                    name,
                    "bakes the card canvas under the image",
                    stats is not None and stats[2] == 255 and stats[3] == 255,
                    f"{stats[0]}x{stats[1]} PNG, alpha {stats[2]}..{stats[3]}"
                    if stats
                    else "image payload is not decodable",
                )
            # The screen model does not parse APC/OSC image payloads, and the
            # backend positions wide glyphs cell by cell, so CJK labels are not
            # contiguous in the byte stream. Layout correctness is covered by
            # the render tests; here the image path is asserted end to end and
            # the ASCII brand proves the welcome card text was painted.
            wait_raw_stable(pty, quiet=1.0, timeout=20)
            raw = bytes(pty.raw)
            self.report.check(name, "welcome text painted", b"CodeLeveler" in raw)
            self.save(name, pty)
        finally:
            self.quit_and_check(name, pty, expected)
            if pty.alive():
                pty.close()

    def run_hidden_terminal(self, name: str, spec: dict) -> None:
        started = time.monotonic()
        pty, _home, _repo, _env = self.open(name, spec)
        try:
            if not self.boot(name, pty):
                self.report.check(name, "boots to the welcome card", False)
                return
            self.report.check(
                name,
                "boots to the welcome card",
                True,
                f"{time.monotonic() - started:.2f}s spawn -> welcome",
            )
            time.sleep(self.args.settle)
            raw = bytes(pty.raw)
            self.report.check(name, "no Kitty APC", b"\x1b_G" not in raw)
            self.report.check(name, "no iTerm2 image", iterm_count(raw) == 0)
            self.report.check(name, "no image placeholder", PLACEHOLDER not in raw)
            text = pty.screen.text()
            for needle in spec.get("labels", ["CodeLeveler", "历史会话", "查看帮助", "选择模型"]):
                self.report.check(name, f"text welcome keeps {needle}", needle in text)
            for line in pty.screen.lines():
                if len(line) > pty.cols:
                    self.report.check(name, "no line overflows the terminal", False, line)
                    break
            self.save(name, pty)
        finally:
            self.quit_and_check(name, pty, "none")
            if pty.alive():
                pty.close()

    def run_lifecycle(self) -> None:
        name = "lifecycle"
        spec = {"term": "xterm-ghostty", "program": "ghostty"}
        started = time.monotonic()
        pty, _home, _repo, _env = self.open(name, spec)
        try:
            if not self.boot(name, pty):
                self.report.check(name, "boots to the welcome card", False)
                return
            if not wait_until(lambda: tx_ids(bytes(pty.raw)), self.args.image_timeout):
                self.report.check(name, "initial image transmitted", False)
                return
            first = tx_ids(bytes(pty.raw))[0]
            self.report.check(
                name,
                "initial image transmitted",
                True,
                f"{time.monotonic() - started:.2f}s spawn -> first Kitty upload",
            )

            before = len(tx_ids(bytes(pty.raw)))
            time.sleep(self.args.idle)
            self.report.check(
                name,
                "idle frames do not re-upload",
                len(tx_ids(bytes(pty.raw))) == before,
            )

            pty.write("draft-input")
            typed = wait_until(lambda: "draft-input" in pty.screen.text(), 5)
            self.report.check(name, "typing reaches the composer", typed)
            pty.write("\x7f" * len("draft-input"))

            resized_at = time.monotonic()
            pty.resize(90, 30)
            replaced = wait_until(
                lambda: first in deleted_ids(bytes(pty.raw))
                and any(item != first for item in tx_ids(bytes(pty.raw))),
                self.args.image_timeout,
            )
            self.report.check(
                name,
                "resize retransmits for the new cell grid",
                replaced,
                f"{time.monotonic() - resized_at:.2f}s resize -> replacement upload",
            )
            wait_raw_stable(pty, quiet=1.0, timeout=20)
            raw = bytes(pty.raw)
            self.report.check(name, "resize deletes the replaced id", first in deleted_ids(raw))
            second = tx_ids(raw)[-1]
            self.report.check(name, "resize allocates a fresh id", second != first)

            for cols, rows in [(100, 32), (120, 42), (80, 24), (110, 40)]:
                pty._winsize(cols, rows)
                pty.screen.resize(cols, rows)
                os.kill(pty.proc.pid, signal.SIGWINCH)
                time.sleep(0.12)
            rapid_at = time.monotonic()
            settled = wait_live_image_settled(pty)
            self.report.check(
                name,
                "rapid resizes settle",
                settled,
                f"{time.monotonic() - rapid_at:.2f}s after the last SIGWINCH",
            )
            raw = bytes(pty.raw)
            ids = tx_ids(raw)
            deletes = deleted_ids(raw)
            last = ids[-1]
            self.report.check(name, "rapid resizes keep transmitting", len(ids) >= 3)
            self.report.check(
                name,
                "every superseded id is deleted and the live one is not",
                all(item in deletes for item in ids[:-1])
                and deletes.count(last) == 0
                and len(set(deletes)) == len(deletes),
            )

            pty.write("after-resize")
            self.report.check(
                name,
                "input still works after resize",
                wait_until(lambda: "after-resize" in pty.screen.text(), 5),
            )
            pty.write("\x7f" * len("after-resize"))
            self.save(name, pty)

            messaged_at = time.monotonic()
            pty.write("hello mock\r")
            retired = wait_until(lambda: last in deleted_ids(bytes(pty.raw)), 20)
            self.report.check(
                name,
                "first message retires the welcome image",
                retired,
                f"{time.monotonic() - messaged_at:.2f}s message -> live id delete",
            )
            self.report.check(
                name,
                "no image upload follows the message",
                len(tx_ids(bytes(pty.raw))) == len(ids),
            )
            pty.wait_for("Hello from the mock provider.", timeout=20)
            self.save(name + "-after-message", pty)
        finally:
            self.quit_and_check(name, pty, "kitty")
            if pty.alive():
                pty.close()

    def run_resume(self) -> None:
        name = "resume"
        spec = {"term": "xterm-ghostty", "program": "ghostty"}
        pty, home, repo, env = self.open(name, spec)
        sid = None
        try:
            if not self.boot(name, pty):
                self.report.check(name, "first run boots", False)
                return
            pty.write("remember this line\r")
            answered = wait_until(
                lambda: "Hello from the mock provider." in pty.screen.text(), 30
            )
            self.report.check(name, "first run gets a reply", answered)
            time.sleep(0.5)
            sid = newest_session(home)
            self.report.check(name, "session persisted", sid is not None, str(sid))
        finally:
            pty.key("\x03", "\x03")
            wait_until(lambda: not pty.alive(), 10)
            if pty.alive():
                pty.close()

        if sid is None:
            self.report.block(name, "resume", "no session row was persisted")
            return
        pty2 = spawn(
            env,
            repo,
            110,
            40,
            ["--session", sid],
            bin_path=self.args.bin,
        )
        try:
            restored = pty2.wait_for("remember this line", timeout=30)
            self.report.check(name, "replays the persisted conversation", restored)
            time.sleep(self.args.image_timeout)
            raw = bytes(pty2.raw)
            self.report.check(name, "resume does not upload the image", b"\x1b_G" not in raw)
            self.report.check(name, "resume shows no welcome card", "让每个模型发挥最大能力" not in pty2.screen.text())
            self.save(name, pty2)
        finally:
            pty2.key("\x03", "\x03")
            wait_until(lambda: not pty2.alive(), 10)
            if pty2.alive():
                pty2.close()

    def run_non_interactive(self) -> None:
        name = "non-interactive"
        _root, _home, repo, base = isolated_env("mock", self.mock, short=True)
        env = env_for(base, term="xterm-ghostty", program="ghostty")
        argv = [
            self.args.bin,
            "--repo",
            repo,
            "tui",
            "--in-process",
            "--permission",
            "full-access",
        ]
        try:
            proc = subprocess.run(
                argv,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=env,
                timeout=15,
            )
        except subprocess.TimeoutExpired as error:
            output = (error.stdout or b"") + (error.stderr or b"")
            self.report.check(name, "non-interactive TUI does not hang", False)
            self.report.check(name, "no image protocol without a TTY", b"\x1b_G" not in output)
            return
        output = proc.stdout + proc.stderr
        with open(os.path.join(self.raws, name + ".bin"), "wb") as fh:
            fh.write(output)
        self.report.check(name, "non-interactive TUI fails fast", proc.returncode != 0)
        self.report.check(name, "no image protocol without a TTY", b"\x1b_G" not in output)
        self.report.check(name, "no iTerm2 protocol without a TTY", iterm_count(output) == 0)


def newest_session(home: str):
    for db in glob.glob(os.path.join(home, "state", "projects", "*", "sessions.db")):
        try:
            con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=1)
            rows = con.execute("SELECT id FROM sessions").fetchall()
            con.close()
        except sqlite3.Error:
            continue
        if rows:
            return rows[-1][0]
    return None


MATRIX = {
    "ghostty": {"term": "xterm-ghostty", "program": "ghostty", "expect": "kitty"},
    "kitty-term": {"term": "xterm-kitty", "expect": "kitty"},
    "iterm2": {"term": "xterm-256color", "program": "iTerm.app", "expect": "iterm"},
    "wezterm": {"term": "xterm-256color", "program": "WezTerm", "expect": "iterm"},
    "ssh-kitty": {"term": "xterm-kitty", "ssh": "10.0.0.1 50000 10.0.0.2 22", "expect": "kitty"},
    "lc-iterm": {"term": "xterm-256color", "lc": "iTerm2", "expect": "iterm"},
    "apple": {"term": "xterm-256color", "program": "Apple_Terminal", "expect": "none"},
    "vscode": {"term": "xterm-256color", "program": "vscode", "expect": "none"},
    "unknown": {"term": "xterm-256color", "expect": "none"},
    "dumb": {"term": "dumb", "program": "iTerm.app", "lc": "iTerm2", "expect": "none"},
    "screen": {"term": "screen-256color", "program": "iTerm.app", "expect": "none"},
    "tmux": {
        "term": "screen-256color",
        "program": "iTerm.app",
        "tmux": "/tmp/tmux-501/default,123,0",
        "expect": "none",
    },
    "tmux-ghostty": {
        "term": "xterm-ghostty",
        "program": "ghostty",
        "tmux": "/tmp/tmux-501/default,123,0",
        "expect": "none",
    },
    "small": {
        "term": "xterm-ghostty",
        "program": "ghostty",
        "size": (48, 12),
        "expect": "none",
        # A 48x12 terminal leaves the welcome card only a few content rows;
        # the deepest visible action is the proof that text still renders.
        "labels": ["CodeLeveler", "选择模型"],
    },
    "no-color": {"term": "xterm-ghostty", "program": "ghostty", "no_color": "1", "expect": "none"},
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", default=DEFAULT_BIN)
    parser.add_argument("--boot-timeout", type=float, default=40)
    parser.add_argument("--image-timeout", type=float, default=40)
    parser.add_argument("--settle", type=float, default=2.5)
    parser.add_argument("--idle", type=float, default=3.0)
    parser.add_argument("--only", default="")
    args = parser.parse_args()
    if not os.path.exists(args.bin):
        print(f"missing binary: {args.bin}", file=sys.stderr)
        return 2
    harness = Harness(args)
    selected = [
        name for name in MATRIX if not args.only or name in args.only.split(",")
    ]
    want_lifecycle = not args.only or "lifecycle" in args.only.split(",")
    want_resume = not args.only or "resume" in args.only.split(",")
    want_non_interactive = not args.only or "non-interactive" in args.only.split(",")
    try:
        for name in selected:
            spec = MATRIX[name]
            if spec["expect"] == "none":
                harness.run_hidden_terminal(name, spec)
            else:
                harness.run_image_terminal(name, spec)
        if want_lifecycle:
            harness.run_lifecycle()
        if want_resume:
            harness.run_resume()
        if want_non_interactive:
            harness.run_non_interactive()
    finally:
        harness.mock.stop()
        path = harness.report.save()
        print(
            f"\n{harness.report.failures} failure(s), "
            f"{harness.report.blocked} blocked; evidence {path}"
        )
    return 1 if harness.report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
