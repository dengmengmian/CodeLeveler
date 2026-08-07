#!/usr/bin/env python3
"""Drive the installed `leveler tui` over a real PTY across a project matrix.

Two lessons are baked in (they make a correct product look broken otherwise):

1. The child's winsize must be set, or `render()` returns before drawing a
   single character (TIOCSWINSZ below).
2. PTY output is cumulative, so assertions must read the CURRENT screen, not
   the concatenation of every frame. A pyte screen gives exactly that.

Turn completion is detected from the product's own busy signal: while a turn
runs the status line paints a braille spinner every 150ms, so "no spinner on
screen AND no new bytes for `quiet` seconds" means the turn really ended —
including during a long silent reasoning phase, which a bytes-only idle check
would misread as completion.
"""

import argparse
import errno
import fcntl
import hashlib
import json
import os
import pty
import re
import select
import signal
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time

import pyte
from wcwidth import wcwidth

ROWS, COLS = 40, 120
SPINNER = set("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")

# A panic reaches the PTY as the runtime's stderr; the TUI itself never prints
# these strings, so any hit is a genuine defect.
PANIC_MARKERS = [
    "panicked at",
    "RUST_BACKTRACE",
    "internal error: entered unreachable code",
    "attempt to subtract with overflow",
    "index out of bounds",
    "called `Option::unwrap()` on a `None` value",
    "called `Result::unwrap()` on an `Err` value",
]

# The TUI's own failure CHROME — not arbitrary words. An assistant answer may
# legitimately contain "崩溃"/"error" while analysing code (a first draft of
# this list matched exactly that and reported a clean run as broken), so match
# only strings the product itself paints on a failed turn: the stop marker
# (i18n `turn_failed`) and the terminal-marker verdicts.
ERROR_MARKERS = ["✗ 已停止", "查看上方错误", "启动失败", "临时提问失败"]

ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]|\x1b\][^\x07]*\x07|\x1b[()][B0]")


class Tui:
    def __init__(self, repo, binary, log_dir, name, extra_args=(), auto_approve=True):
        self.repo = repo
        self.binary = binary
        self.name = name
        self.extra_args = list(extra_args)
        # Approvals OFF (auto_approve=True) is right for unattended happy-path
        # runs; a test that means to exercise the approval overlay MUST turn it
        # off. This was hardcoded once, which silently made every "was the
        # deletion gated?" assertion meaningless — the runtime was auto-
        # approving the whole time.
        self.auto_approve = auto_approve
        self.raw = bytearray()
        self.log_path = os.path.join(log_dir, f"{name}.raw")
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.pid = None
        self.fd = None
        self.dead = False
        self.emulator_errors = []

    def start(self):
        pid, fd = pty.fork()
        if pid == 0:  # child
            os.chdir(self.repo)
            env = dict(os.environ)
            env["TERM"] = "xterm-256color"
            env["LANG"] = env.get("LANG", "en_US.UTF-8")
            env["NO_COLOR"] = "0"
            args = [self.binary, "--repo", self.repo, "tui", "--in-process"]
            if self.auto_approve:
                args.append("--auto-approve")
            args += self.extra_args
            try:
                os.execve(self.binary, args, env)
            except Exception:
                os._exit(127)
        self.pid = pid
        self.fd = fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        return self

    # ── io ────────────────────────────────────────────────────────────────
    def _read_available(self, timeout):
        """Read whatever is ready; return True if any bytes arrived."""
        if self.fd is None:
            self.dead = True
            return False
        got = False
        deadline = time.time() + timeout
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                break
            try:
                r, _, _ = select.select([self.fd], [], [], max(0.02, min(remaining, 0.2)))
            except (OSError, ValueError):
                self.dead = True
                break
            if not r:
                break
            try:
                chunk = os.read(self.fd, 65536)
            except OSError as e:
                if e.errno in (errno.EIO, errno.EBADF):
                    self.dead = True
                    break
                raise
            if not chunk:
                self.dead = True
                break
            self.raw.extend(chunk)
            try:
                self.stream.feed(chunk)
            except Exception as exc:  # a pyte quirk is not a product defect
                self.emulator_errors.append(repr(exc))
            got = True
        return got

    def send(self, data):
        if self.fd is None:
            self.dead = True
            return
        if isinstance(data, str):
            data = data.encode()
        try:
            os.write(self.fd, data)
        except OSError:
            self.dead = True

    def resize(self, rows, cols):
        """Resize the child's terminal AND the emulator together — feeding a
        200-column redraw into a 120-column emulator is a driver bug that
        surfaces as a product one."""
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.screen.resize(rows, cols)
        os.kill(self.pid, signal.SIGWINCH)

    def frame(self):
        """Render the screen without pyte's `display`.

        `pyte.Screen.display` calls `wcwidth(char[0])`, which raises on an
        empty cell — and resizing a screen holding CJK text leaves exactly
        such orphaned wide-char continuation cells. That is an emulator
        quirk, not a product defect, so read the buffer directly and treat an
        empty cell as a space.
        """
        lines = []
        for y in range(self.screen.lines):
            row = self.screen.buffer[y]
            out = []
            skip_next = False
            for x in range(self.screen.columns):
                if skip_next:
                    skip_next = False
                    continue
                data = row[x].data
                if not data:
                    out.append(" ")
                    continue
                out.append(data)
                # A double-width glyph occupies two cells and pyte leaves the
                # second one empty. Emitting a space for that continuation
                # cell turns "测试" into "测 试" and breaks every CJK
                # assertion — which is exactly what it did.
                if wcwidth(data[0]) == 2:
                    skip_next = True
            lines.append("".join(out).rstrip())
        return lines

    def frame_text(self):
        return "\n".join(self.frame())

    def busy(self):
        return any(ch in SPINNER for ch in self.frame_text())

    def settle(self, timeout=20.0, quiet=1.0):
        """Wait until the screen stops changing and no turn is running."""
        start = time.time()
        last_change = time.time()
        while time.time() - start < timeout:
            if self._read_available(0.25):
                last_change = time.time()
            if self.dead:
                return "dead"
            idle_for = time.time() - last_change
            if idle_for >= quiet and not self.busy():
                return "idle"
        return "busy" if self.busy() else "timeout"

    def _close_fd(self):
        """Idempotent fd release. Every exit path routes through this: the
        `dead` early-return used to skip it, leaking one PTY fd per project
        (measured: /dev/fd grew 5 -> 25 over 20 rounds)."""
        if self.fd is not None:
            try:
                os.close(self.fd)
            except OSError:
                pass
            self.fd = None

    def kill_hard(self):
        """SIGKILL the child — a real crash, not a graceful exit.

        The reopen/recovery round is meaningless against a clean shutdown:
        the whole question is what survives when the process never got to
        run its teardown.
        """
        if self.pid is None:
            return
        try:
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass
        self.dead = True
        self._close_fd()
        self._flush_log()

    def _flush_log(self):
        os.makedirs(os.path.dirname(self.log_path), exist_ok=True)
        with open(self.log_path, "wb") as fh:
            fh.write(bytes(self.raw))

    def close(self):
        if self.pid is None or self.dead:
            self._close_fd()
            self._flush_log()
            return
        self.send(b"\x03")
        time.sleep(0.25)
        self.send(b"\x03")
        deadline = time.time() + 6
        while time.time() < deadline:
            self._read_available(0.2)
            try:
                pid, _ = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    break
            except ChildProcessError:
                break
        else:
            try:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass
        self._close_fd()
        self._flush_log()

    # ── diagnostics ───────────────────────────────────────────────────────
    def raw_text(self):
        return ANSI_RE.sub("", self.raw.decode("utf-8", "replace"))

    def panics(self):
        text = self.raw_text()
        return [m for m in PANIC_MARKERS if m in text]


def assert_disposable(path):
    """Refuse to drive a workspace that is not a throwaway.

    The agent edits, deletes, and runs commands in here. Pointing that at the
    fixture corpus itself is how the corpus ends up permanently dirty and the
    matrix stops being repeatable.
    """
    real = os.path.realpath(path)
    fixtures = os.path.realpath(os.path.join(repo_root(), "fixtures"))
    if real.startswith(fixtures + os.sep):
        raise RuntimeError(
            f"refusing to run against the fixture corpus in place: {real}\n"
            "cases must run in a disposable workspace (see prepare_workspace)"
        )


def run_project(spec, binary, log_dir, prompts):
    """One project: 11 rounds mixing UI interaction and real model turns."""
    assert_disposable(spec["path"])
    name = spec["name"]
    tui = Tui(spec["path"], binary, log_dir, name).start()
    findings = []
    rounds = []

    def note(kind, detail, frame=None):
        findings.append({"kind": kind, "detail": detail,
                         "frame": frame[-1200:] if frame else None})

    def record(label, state, extra=None):
        entry = {"round": len(rounds) + 1, "label": label, "state": state}
        if extra:
            entry.update(extra)
        rounds.append(entry)
        return entry

    try:
        # R1 — startup paints something.
        state = tui.settle(timeout=45, quiet=1.2)
        frame = tui.frame_text()
        record("startup", state, {"chars": len(frame.strip())})
        if state == "dead":
            note("startup-died", "the TUI exited before painting", frame)
            return {"name": name, "rounds": rounds, "findings": findings,
                    "type": spec["type"]}
        if len(frame.strip()) < 40:
            note("blank-screen", f"only {len(frame.strip())} chars painted", frame)

        # R2 — slash menu opens and Esc dismisses it.
        tui.send("/")
        tui.settle(timeout=10, quiet=0.5)
        opened = tui.frame_text()
        if "/" not in opened:
            note("slash-menu", "typing / produced no visible change", opened)
        tui.send("\x1b")
        tui.settle(timeout=10, quiet=0.5)
        record("slash-menu", "ok", {"listed": opened.count("/")})

        # R3 — permission cycling (Shift+Tab).
        before = tui.frame_text()
        tui.send("\x1b[Z")
        tui.settle(timeout=10, quiet=0.5)
        after = tui.frame_text()
        record("shift-tab", "ok", {"changed": before != after})
        if before == after:
            note("shift-tab-inert", "Shift+Tab changed nothing on screen", after)

        # R4 — CJK input reaches the composer.
        tui.send("测试中文输入")
        tui.settle(timeout=10, quiet=0.5)
        typed = tui.frame_text()
        if "测试中文" not in typed:
            note("cjk-input", "Chinese text did not appear in the composer", typed)
        # clear it
        tui.send("\x15")  # ctrl-u kill line
        tui.settle(timeout=8, quiet=0.4)
        record("cjk-input", "ok")

        # R5..R7 — three REAL model turns against the real repository.
        for i, prompt in enumerate(prompts, start=1):
            tui.send(prompt + "\r")
            state = tui.settle(timeout=spec.get("turn_timeout", 420), quiet=2.5)
            frame = tui.frame_text()
            entry = record(f"model-turn-{i}", state, {"prompt": prompt[:60]})
            if state in ("timeout", "busy"):
                note("turn-hung", f"turn {i} still busy after timeout: {prompt[:60]}", frame)
            if state == "dead":
                note("died-mid-turn", f"process died during turn {i}", frame)
                break
            for marker in ERROR_MARKERS:
                if marker in frame:
                    note("error-on-screen", f"turn {i} shows {marker!r}", frame)
                    break
            entry["screen_chars"] = len(frame.strip())

        if tui.dead:
            return {"name": name, "rounds": rounds, "findings": findings,
                    "type": spec["type"]}

        # R8 — expand the latest tool group.
        tui.send("\x0f")  # ctrl-o
        tui.settle(timeout=12, quiet=0.6)
        record("ctrl-o", "ok")

        # R9 — scroll the transcript.
        tui.send("\x1b[5~")  # PgUp
        tui.settle(timeout=10, quiet=0.5)
        scrolled = tui.frame_text()
        tui.send("\x1b[6~")  # PgDn
        tui.settle(timeout=10, quiet=0.5)
        record("scroll", "ok", {"changed": scrolled != tui.frame_text()})

        # R10 — a slash screen round trip (diff view).
        tui.send("/diff\r")
        tui.settle(timeout=25, quiet=0.8)
        diff_frame = tui.frame_text()
        tui.send("\x1b")
        tui.settle(timeout=10, quiet=0.5)
        record("diff-screen", "ok", {"chars": len(diff_frame.strip())})
        for marker in PANIC_MARKERS:
            if marker in diff_frame:
                note("panic-in-diff", marker, diff_frame)

        # R11 — final render sanity: the composer must still be usable.
        tui.send("最后一轮检查")
        tui.settle(timeout=10, quiet=0.5)
        final = tui.frame_text()
        if "最后一轮" not in final:
            note("composer-dead", "composer stopped accepting input after the run", final)
        record("final-input", "ok")

    finally:
        tui.close()
        for marker in tui.panics():
            note("panic", marker)
        if tui.dead and not any(f["kind"].startswith("startup") for f in findings):
            pass

    return {"name": name, "type": spec["type"], "rounds": rounds,
            "findings": findings, "raw_log": tui.log_path}


class Workspace:
    """A throwaway checkout the agent may freely modify.

    Running against the fixture repositories directly makes the matrix
    unrepeatable: the second run inherits the first run's edits, and a real
    upstream corpus ends up permanently dirty. Every case gets its own copy
    pinned to a fixed ref, and it is discarded afterwards.
    """

    def __init__(self, path, base_ref, cleanup):
        self.path = path
        self.base_ref = base_ref
        self._cleanup = cleanup

    def discard(self):
        try:
            self._cleanup()
        except Exception as exc:  # cleanup must never mask a real result
            print(f"    warning: workspace cleanup failed: {exc!r}", flush=True)


def _git(args, cwd=None):
    return subprocess.run(
        ["git", *args], cwd=cwd, capture_output=True, text=True, check=False
    )


def prepare_workspace(spec, tmp_root):
    """Check `spec` out into a disposable workspace and return it.

    Git projects use a detached worktree at a pinned ref, so the base is the
    same on every run no matter what a previous run left behind. Non-git
    directories (a deliberate fixture shape) are copied.
    """
    source = spec["path"]
    dest = os.path.join(tmp_root, spec["name"])
    if os.path.isdir(os.path.join(source, ".git")):
        requested = spec.get("base_ref", "HEAD")
        resolved = _git(["rev-parse", requested], cwd=source).stdout.strip()
        if not resolved:
            raise RuntimeError(f"{spec['name']}: cannot resolve base ref {requested!r}")
        added = _git(["worktree", "add", "--detach", dest, resolved], cwd=source)
        if added.returncode != 0:
            raise RuntimeError(
                f"{spec['name']}: git worktree add failed: {added.stderr.strip()}"
            )

        def cleanup():
            _git(["worktree", "remove", "--force", dest], cwd=source)
            _git(["worktree", "prune"], cwd=source)

        return Workspace(dest, resolved, cleanup)

    shutil.copytree(source, dest)
    return Workspace(dest, "(not version controlled)", lambda: shutil.rmtree(dest, True))


def run_metadata(binary):
    """What a result file needs to be re-checkable later."""
    root = repo_root()
    digest = hashlib.sha256()
    with open(binary, "rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            digest.update(chunk)
    return {
        "leveler_commit": _git(["rev-parse", "HEAD"], cwd=root).stdout.strip(),
        "leveler_dirty": bool(_git(["status", "--porcelain"], cwd=root).stdout.strip()),
        "binary": binary,
        "binary_sha256": digest.hexdigest(),
        "binary_version": subprocess.run(
            [binary, "--version"], capture_output=True, text=True, check=False
        ).stdout.strip(),
        "command": " ".join(sys.argv),
    }


def repo_root():
    """The checkout this file lives in — matrix paths are relative to it."""
    return os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))


def load_matrix(path, only=None):
    """Read the matrix and resolve `path` entries relative to the checkout.

    Absolute paths in the file would pin the matrix to one machine; keeping
    them relative is what lets a fresh clone or CI run it at all.
    """
    with open(path) as fh:
        matrix = json.load(fh)
    root = repo_root()
    for spec in matrix:
        spec["path"] = os.path.normpath(os.path.join(root, spec["path"]))
    if only:
        wanted = set(only.split(","))
        known = {m["name"] for m in matrix}
        missing = sorted(wanted - known)
        if missing:
            raise ValueError(f"unknown matrix project(s): {', '.join(missing)}")
        matrix = [m for m in matrix if m["name"] in wanted]
    return matrix


def report(results, expected_projects, min_rounds):
    """Print a structured summary and return a SHELL EXIT CODE.

    Returning 0 unconditionally (as this once did) makes a run that found
    defects, lost a project, or crashed in the driver look exactly like a
    clean one. Anything short of "every project ran its rounds and nothing
    was found" is a failure.
    """
    findings = [(r["name"], f["kind"], f["detail"]) for r in results for f in r["findings"]]
    short = [(r["name"], len(r["rounds"])) for r in results if len(r["rounds"]) < min_rounds]
    missing = expected_projects - len(results)

    print(f"\nprojects: {len(results)}/{expected_projects}", flush=True)
    print(f"findings: {len(findings)}", flush=True)
    for name, kind, detail in findings:
        print(f"  - {name}: {kind}: {detail[:160]}", flush=True)
    for name, n in short:
        print(f"  - {name}: only {n} rounds (expected >= {min_rounds})", flush=True)

    failed = bool(findings) or bool(short) or missing > 0
    print("RESULT:", "FAIL" if failed else "PASS", flush=True)
    return 1 if failed else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--matrix", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--log-dir", required=True)
    ap.add_argument("--binary", default=os.path.expanduser("~/.cargo/bin/leveler"))
    ap.add_argument("--only", default=None, help="comma-separated project names")
    args = ap.parse_args()

    os.makedirs(args.log_dir, exist_ok=True)
    matrix = load_matrix(args.matrix, args.only)

    results = []
    meta = run_metadata(args.binary)
    tmp_root = tempfile.mkdtemp(prefix="leveler-matrix-")
    print(f"workspaces: {tmp_root}", flush=True)
    print(f"leveler: {meta['leveler_commit'][:12]}"
          + ("+dirty" if meta["leveler_dirty"] else "")
          + f"  binary: {meta['binary_version']}", flush=True)
    for spec in matrix:
        started = time.time()
        print(f"[{len(results)+1}/{len(matrix)}] {spec['name']} ({spec['type']})",
              flush=True)
        workspace = None
        try:
            workspace = prepare_workspace(spec, tmp_root)
            isolated = dict(spec, path=workspace.path)
            res = run_project(isolated, args.binary, args.log_dir, spec["prompts"])
            res["base_ref"] = workspace.base_ref
        except Exception as e:  # driver bug must not look like a product bug
            res = {"name": spec["name"], "type": spec["type"], "rounds": [],
                   "findings": [{"kind": "driver-error", "detail": repr(e)}]}
        finally:
            if workspace is not None:
                workspace.discard()
        res["seconds"] = round(time.time() - started, 1)
        results.append(res)
        bad = [f for f in res["findings"]]
        print(f"    {res['seconds']}s  rounds={len(res['rounds'])}  findings={len(bad)}"
              + ("  " + "; ".join(f["kind"] for f in bad) if bad else ""), flush=True)
        with open(args.out, "w") as fh:
            json.dump({"meta": meta, "results": results}, fh,
                      ensure_ascii=False, indent=2)

    shutil.rmtree(tmp_root, ignore_errors=True)
    return report(results, len(matrix), min_rounds=11)


if __name__ == "__main__":
    sys.exit(main())
