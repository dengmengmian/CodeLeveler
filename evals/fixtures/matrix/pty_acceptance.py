#!/usr/bin/env python3
"""Drive the release `leveler` TUI over a real PTY and keep the sessions it writes.

`tui_drive.py` / `tui_stress.py` / `tui_forty.py` answer "does the assembled
binary survive a terminal". This adds the two things a pre-release acceptance
needs on top of that:

1. **Isolation.** Every run gets its own `LEVELER_HOME` and its own throwaway
   worktree, so a driven run never shares a session store, a runtime lock, or a
   socket with a person using the product at the same time.
2. **A durable trail.** Each run's `sessions.db` is copied out when it ends, so
   the same trajectory can afterwards be pushed back through the real bridge,
   reducer and renderer (`leveler-cli --test real_session_replay`). The PTY says
   what the terminal did; the replay says whether the projection was true.

It also samples RSS and *interval* CPU (from cumulative CPU time, not `ps`'s
since-launch average, which cannot see a busy loop that starts late).

    python3 evals/fixtures/matrix/pty_acceptance.py --stage A \\
        --lab /path/to/lab --binary /path/to/leveler --out out/a.json
"""

import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from tui_drive import (  # noqa: E402
    COLS,
    ERROR_MARKERS,
    PANIC_MARKERS,
    ROWS,
    Tui,
    newest_session_id,
    session_facts,
    state_dir_for,
)

# The composer placeholder. Only painted while the composer is empty AND the
# session has no transcript, so it anchors a startup screen and nothing else.
COMPOSER_HINT = "输入消息"

CPU_TIME_RE = re.compile(r"(?:(\d+)-)?(?:(\d+):)?(\d+):(\d+(?:\.\d+)?)")


def cpu_seconds(text):
    """Parse `ps -o time=` (`[DD-][HH:]MM:SS.ss`) into seconds."""
    m = CPU_TIME_RE.search(text.strip())
    if not m:
        return None
    days, hours, minutes, seconds = m.groups()
    return (
        int(days or 0) * 86400
        + int(hours or 0) * 3600
        + int(minutes) * 60
        + float(seconds)
    )


def sample_proc(pid):
    """(rss_kb, cumulative_cpu_seconds) for `pid`, or (None, None)."""
    out = subprocess.run(
        ["ps", "-o", "rss=,time=", "-p", str(pid)],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.strip()
    if not out:
        return None, None
    parts = out.split(None, 1)
    if len(parts) != 2:
        return None, None
    try:
        return int(parts[0]), cpu_seconds(parts[1])
    except ValueError:
        return None, None


class Sampler:
    """Background RSS / interval-CPU sampling for one process.

    `ps`'s `%cpu` is an average over the process's whole life, so a busy loop
    that starts in minute nine is invisible in it. Cumulative CPU time
    differenced over the sampling interval is not.
    """

    def __init__(self, pid, interval=5.0):
        self.pid = pid
        self.interval = interval
        self.samples = []
        self._stop = threading.Event()
        self._thread = None

    def start(self):
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()
        return self

    def _loop(self):
        prev_t = prev_cpu = None
        while not self._stop.is_set():
            now = time.time()
            rss, cpu = sample_proc(self.pid)
            if rss is not None:
                pct = None
                if prev_cpu is not None and now > prev_t:
                    pct = round(100.0 * (cpu - prev_cpu) / (now - prev_t), 1)
                self.samples.append(
                    {"t": round(now, 2), "rss_kb": rss, "cpu_pct": pct, "phase": self.phase}
                )
                prev_t, prev_cpu = now, cpu
            self._stop.wait(self.interval)

    phase = "idle"

    def mark(self, phase):
        self.phase = phase

    def stop(self):
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=2)
        return self.samples

    def summary(self):
        if not self.samples:
            return {}
        rss = [s["rss_kb"] for s in self.samples]
        cpu = [s["cpu_pct"] for s in self.samples if s["cpu_pct"] is not None]
        by_phase = {}
        for s in self.samples:
            if s["cpu_pct"] is None:
                continue
            by_phase.setdefault(s["phase"], []).append(s["cpu_pct"])
        return {
            "samples": len(self.samples),
            "rss_start_kb": rss[0],
            "rss_peak_kb": max(rss),
            "rss_end_kb": rss[-1],
            "rss_growth_kb": rss[-1] - rss[0],
            "cpu_max_pct": max(cpu) if cpu else None,
            "cpu_median_pct": sorted(cpu)[len(cpu) // 2] if cpu else None,
            "cpu_by_phase_max": {k: max(v) for k, v in by_phase.items()},
            "cpu_by_phase_median": {
                k: sorted(v)[len(v) // 2] for k, v in by_phase.items()
            },
        }


class Lab:
    """Per-run isolation: its own LEVELER_HOME, worktree, and kept session store."""

    def __init__(self, root, binary, config_source):
        self.root = os.path.abspath(root)
        self.binary = binary
        self.config_source = config_source
        for sub in ("home", "ws", "sessions", "logs"):
            os.makedirs(os.path.join(self.root, sub), exist_ok=True)

    def home_for(self, run_id):
        home = os.path.join(self.root, "home", run_id)
        os.makedirs(home, exist_ok=True)
        if self.config_source and os.path.exists(self.config_source):
            shutil.copy(self.config_source, os.path.join(home, "config.toml"))
        return home

    def worktree(self, source, run_id, base_ref=None):
        """A detached worktree at a pinned ref, or a copy for a non-git tree."""
        dest = os.path.join(self.root, "ws", run_id)
        if os.path.isdir(dest):
            shutil.rmtree(dest, ignore_errors=True)
        if os.path.isdir(os.path.join(source, ".git")):
            # Removing the directory does not deregister the worktree, so a
            # rerun under the same id was refused by `git worktree add` and the
            # run failed before it had a workspace at all.
            subprocess.run(["git", "worktree", "prune"], cwd=source,
                           capture_output=True, check=False)
            ref = base_ref or "HEAD"
            resolved = subprocess.run(
                ["git", "rev-parse", ref], cwd=source, capture_output=True, text=True
            ).stdout.strip()
            add = subprocess.run(
                ["git", "worktree", "add", "--detach", dest, resolved],
                cwd=source,
                capture_output=True,
                text=True,
            )
            if add.returncode != 0:
                raise RuntimeError(f"worktree add failed: {add.stderr.strip()}")
            return dest, resolved, source
        shutil.copytree(source, dest)
        return dest, "(not version controlled)", None

    def keep_sessions(self, repo, run_id):
        """Copy the run's store out before the home is reused or removed."""
        src_dir = state_dir_for(repo)
        if not src_dir:
            return None
        dest = os.path.join(self.root, "sessions", run_id)
        os.makedirs(dest, exist_ok=True)
        kept = None
        for name in ("sessions.db", "sessions.db-wal", "sessions.db-shm"):
            src = os.path.join(src_dir, name)
            if os.path.exists(src):
                shutil.copy(src, os.path.join(dest, name))
                if name == "sessions.db":
                    kept = os.path.join(dest, name)
        return kept


class Run:
    """One PTY run: launch, drive, sample, keep the store, report."""

    def __init__(self, lab, project, run_id, base_ref=None, auto_approve=True,
                 mode=None, session=None, home=None, rows=ROWS, cols=COLS):
        self.lab = lab
        self.project = project
        self.run_id = run_id
        self.base_ref = base_ref
        self.auto_approve = auto_approve
        self.mode = mode
        self.session = session
        self.rows, self.cols = rows, cols
        self.findings = []
        self.rounds = []
        self.home = home or lab.home_for(run_id)
        self.ws = None
        self.worktree_source = None
        self.resolved_ref = None
        self.tui = None
        self.sampler = None
        self.started_at = None
        self.ended_at = None
        self.screens = {}
        self.mid_turn = []

    # ── lifecycle ─────────────────────────────────────────────────────────
    def start(self, reuse_ws=None):
        if reuse_ws:
            self.ws, self.resolved_ref, self.worktree_source = reuse_ws, None, None
        else:
            self.ws, self.resolved_ref, self.worktree_source = self.lab.worktree(
                self.project["path"], self.run_id, self.base_ref
            )
        os.environ["LEVELER_HOME"] = self.home
        extra = []
        if self.mode:
            extra += ["--permission", self.mode]
        if self.session:
            extra += ["--session", self.session]
        # Tui reads ROWS/COLS from the module; drive a non-default size by
        # resizing right after launch rather than forking the module state.
        self.tui = Tui(
            self.ws,
            self.lab.binary,
            os.path.join(self.lab.root, "logs"),
            self.run_id,
            extra_args=extra,
            auto_approve=self.auto_approve,
        ).start()
        self.started_at = time.time()
        self.sampler = Sampler(self.tui.pid).start()
        if (self.rows, self.cols) != (ROWS, COLS):
            self.tui.resize(self.rows, self.cols)
        return self

    def note(self, kind, detail, frame=None):
        self.findings.append(
            {"kind": kind, "detail": detail, "frame": (frame or "")[-1500:] or None}
        )

    def record(self, label, state, extra=None):
        entry = {"round": len(self.rounds) + 1, "label": label, "state": state}
        if extra:
            entry.update(extra)
        self.rounds.append(entry)
        return entry

    def keep_screen(self, label):
        self.screens[label] = self.tui.frame_text()
        return self.screens[label]

    def scan(self, frame, where):
        for marker in PANIC_MARKERS:
            if marker in frame:
                self.note("panic", f"{marker} ({where})", frame)
        for marker in ERROR_MARKERS:
            if marker in frame:
                self.note("error-on-screen", f"{marker} ({where})", frame)

    def settle(self, timeout, quiet=2.0, phase=None):
        if phase and self.sampler:
            self.sampler.mark(phase)
        state = self.tui.settle(timeout=timeout, quiet=quiet)
        if self.sampler:
            self.sampler.mark("idle")
        return state

    def turn(self, prompt, timeout=600, label=None, quiet=2.5, sample_every=None):
        """One real model turn, typed through the PTY like a person would.

        `sample_every` keeps a screen at that cadence for the whole turn. A long
        task's only evidence otherwise is its last frame, which says nothing
        about whether the interface stayed alive in the middle of it.
        """
        label = label or "turn"
        self.tui.send(prompt + "\r")
        t0 = time.time()
        if sample_every:
            state = self._settle_sampling(timeout, quiet, label, sample_every, t0)
        else:
            state = self.settle(timeout=timeout, quiet=quiet, phase="turn")
        frame = self.keep_screen(label)
        entry = self.record(
            label, state, {"prompt": prompt[:80], "seconds": round(time.time() - t0, 1)}
        )
        if state in ("busy", "timeout"):
            self.note("turn-hung", f"{label} never settled: {prompt[:60]}", frame)
        elif state == "dead":
            self.note("died-mid-turn", f"process died during {label}", frame)
        else:
            self.scan(frame, label)
        return entry

    def _settle_sampling(self, timeout, quiet, label, every, t0):
        """Settle in slices, keeping a screen and a note of liveness each slice."""
        if self.sampler:
            self.sampler.mark("turn")
        deadline = time.time() + timeout
        n = 0
        while time.time() < deadline:
            slice_timeout = min(every, max(1.0, deadline - time.time()))
            state = self.tui.settle(timeout=slice_timeout, quiet=quiet)
            if state in ("idle", "dead"):
                if self.sampler:
                    self.sampler.mark("idle")
                return state
            n += 1
            screen = self.tui.frame_text()
            self.screens[f"{label}-t{int(time.time() - t0)}s"] = screen
            # What a person looks at while the model is thinking: the screen
            # must stay alive and must not claim a command is running when the
            # turn is only waiting.
            self.mid_turn.append({
                "at_seconds": round(time.time() - t0, 1),
                "chars": len(screen.strip()),
                "busy_marker": self.tui.busy(),
                "blank": len(screen.strip()) < 40,
            })
        if self.sampler:
            self.sampler.mark("idle")
        return "busy" if self.tui.busy() else "timeout"

    def composer_alive(self, where):
        """The composer must still take input after whatever just happened."""
        probe = f"探针{uuid.uuid4().hex[:4]}"
        self.tui.send(probe)
        self.settle(timeout=12, quiet=0.5)
        ok = probe in self.tui.frame_text()
        self.tui.send("\x15")  # ctrl-u: leave the composer clean
        self.settle(timeout=8, quiet=0.4)
        if not ok:
            self.note("composer-dead", f"composer stopped accepting input {where}",
                      self.tui.frame_text())
        return ok

    def finish(self):
        if self.ws is None:
            # The run never got a workspace (a driver error before start).
            # Report that rather than crashing the whole stage on top of it.
            self.ended_at = time.time()
            return {
                "run_id": self.run_id,
                "project": self.project["name"],
                "type": self.project.get("type"),
                "rounds": self.rounds,
                "findings": self.findings,
                "metrics": {},
                "samples": [],
                "screens": {},
                "wall_seconds": 0.0,
                "session_id": None,
                "sessions_db": None,
            }
        self.session_id = newest_session_id(self.ws)
        facts = session_facts(self.ws, self.session_id) if self.session_id else {}
        self.tui.close()
        self.ended_at = time.time()
        samples = self.sampler.stop() if self.sampler else []
        db = self.lab.keep_sessions(self.ws, self.run_id)
        for marker in self.tui.panics():
            self.note("panic-in-stream", marker)
        if self.tui.emulator_errors:
            self.record("emulator", "note", {"errors": self.tui.emulator_errors[:3]})
        return {
            "run_id": self.run_id,
            "project": self.project["name"],
            "type": self.project.get("type"),
            "repo": self.ws,
            "base_ref": self.resolved_ref,
            "home": self.home,
            "session_id": self.session_id,
            "session_facts": facts,
            "sessions_db": db,
            "pty_size": [self.rows, self.cols],
            "auto_approve": self.auto_approve,
            "permission_mode": self.mode,
            "started": self.started_at,
            "ended": self.ended_at,
            "wall_seconds": round((self.ended_at or 0) - (self.started_at or 0), 1),
            "rounds": self.rounds,
            "mid_turn": self.mid_turn,
            "findings": self.findings,
            "metrics": self.sampler.summary() if self.sampler else {},
            "samples": samples,
            "raw_log": self.tui.log_path,
            "screens": self.screens,
        }

    def discard_worktree(self):
        if self.worktree_source:
            subprocess.run(
                ["git", "worktree", "remove", "--force", self.ws],
                cwd=self.worktree_source, capture_output=True, check=False,
            )
            subprocess.run(["git", "worktree", "prune"], cwd=self.worktree_source,
                           capture_output=True, check=False)


def binary_identity(binary):
    import hashlib

    digest = hashlib.sha256()
    with open(binary, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            digest.update(chunk)
    return {
        "path": binary,
        "sha256": digest.hexdigest(),
        "version": subprocess.run(
            [binary, "--version"], capture_output=True, text=True, check=False
        ).stdout.strip(),
    }


def write_result(path, payload):
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "w") as fh:
        json.dump(payload, fh, ensure_ascii=False, indent=2)
