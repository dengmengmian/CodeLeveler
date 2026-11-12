#!/usr/bin/env python3
"""TUI render truth: reproduce and grade incremental-paint artifacts on a real PTY.

The defect under test is a *rendering* one: the terminal screen after a real
streaming run must equal the screen after a single full paint of the same
logical content. This harness drives the real `leveler` binary on a real PTY
with a scripted OpenAI-wire provider whose deltas are written one SSE frame at
a time, so the runtime, reducer, markdown layout, renderer, buffer diff and
terminal all run exactly as they do against a live model — only the token
source is deterministic.

Modes:
  probe        stream a fixture, snapshot the screen whenever the stream is
               paused at a known offset, and check the contract that no raw
               tab/control byte reaches the terminal and no frame mixes two
               layouts.
  equivalence  stream with pauses, then for each chosen offset boot a second
               TUI that receives the same prefix as ONE delta, held at the
               same streaming state. The two screens must be identical after
               normalising the known dynamic chrome (clock, elapsed, token
               estimate, repo path, spinner phase).

Evidence (frames, raw PTY bytes, assertions) lands in
`target/tui-render-truth/<run>/`.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import shutil
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import pty_dogfood as H  # noqa: E402

ROOT = H.ROOT
BIN = H.BIN
FIXTURE = os.path.join(ROOT, "testdata", "tui_render_truth", "streaming_answer.md")
EVIDENCE = os.path.join(ROOT, "target", "tui-render-truth")

# Residue signatures from the original report. These are tails of *this*
# fixture's own code lines; they appear legitimately once the prefix reaches
# them, so they are diagnostics, never the pass/fail oracle.
RESIDUE = ("]))", 'pt"', "il)")

SPINNERS = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"


def chunks_of(text: str, size: int, seed: int | None = None) -> list[str]:
    """Split a fixture into provider deltas (character slicing, like JSON)."""
    if seed is None:
        return [text[i : i + size] for i in range(0, len(text), size)]
    rng = random.Random(seed)
    out = []
    i = 0
    while i < len(text):
        n = rng.randint(1, size)
        out.append(text[i : i + n])
        i += n
    return out


def delta_frame(content: str) -> str:
    return json.dumps({"choices": [{"delta": {"content": content}}]})


def stop_frame() -> str:
    return json.dumps({"choices": [{"delta": {}, "finish_reason": "stop"}]})


class StreamScript:
    """A chunk script plus the character offsets where the provider pauses.

    `paused_at` is read by the harness thread; the handler blocks until the
    harness acknowledges the snapshot, so a frame is never scraped while the
    stream is still moving. The pause is bounded so a crashed harness cannot
    wedge the provider forever.
    """

    ACK_TIMEOUT = 60.0

    def __init__(self, text: str, chunks: list[str], pause_offsets: set[int]):
        self.text = text
        self.chunks = chunks
        self.pause_offsets = pause_offsets
        self.paused_at: int | None = None
        self.ack = -1
        self.finished = False
        self.cond = threading.Condition()

    def frames(self) -> list[str]:
        return [delta_frame(c) for c in self.chunks] + [stop_frame()]

    def hook(self, cum: int):
        """Mock-provider callback: after `cum` content characters were flushed."""
        if cum in self.pause_offsets:
            with self.cond:
                self.paused_at = cum
                self.cond.notify_all()
                deadline = time.time() + self.ACK_TIMEOUT
                while self.ack != cum and time.time() < deadline:
                    self.cond.wait(0.1)
        if cum >= len(self.text):
            with self.cond:
                self.finished = True
                self.cond.notify_all()

    def acknowledge(self, cum: int):
        with self.cond:
            self.ack = cum
            self.cond.notify_all()

    def wait_pause(self, after: int | None, timeout: float) -> int | None:
        end = time.time() + timeout
        with self.cond:
            while time.time() < end:
                if self.paused_at is not None and self.paused_at != after:
                    return self.paused_at
                if self.finished:
                    return None
                self.cond.wait(0.05)
        return None

    def done(self) -> bool:
        with self.cond:
            return self.finished


def offsets_after(chunks: list[str]) -> list[int]:
    out = []
    n = 0
    for c in chunks:
        n += len(c)
        out.append(n)
    return out


def start_tui(bin_path: str, cols: int, rows: int):
    mock = H.MockProvider()
    root, home, repo, env = H.isolated_env("mock", mock)
    pty_ = H.spawn(env, repo, cols, rows, [], bin_path=bin_path)
    return mock, root, env, pty_


def wait_boot(pty_: H.Pty, timeout: float = 60.0) -> bool:
    return pty_.wait_for("leveler", timeout=timeout) or pty_.wait_stable(1.0, timeout)


def submit(pty_: H.Pty, prompt: str):
    pty_.write(prompt)
    time.sleep(0.2)
    pty_.key("\r")


def dump_frame(path: str, rows: list[str]):
    with open(path, "w") as fh:
        fh.write("\n".join(rows))
        fh.write("\n")


def control_bytes(raw: bytes) -> dict[str, int]:
    """Count raw control bytes a correct frame stream never emits.

    ESC/CR/LF are the escape stream itself. A TAB is the desync signature: it
    can only come from a cell symbol, and the terminal expands it to a tab
    stop while ratatui counts one column. Remaining C0 bytes have no business
    on the wire either.
    """
    counts = {
        "tab": raw.count(b"\t"),
        "nul": raw.count(b"\x00"),
        "bel": raw.count(b"\x07"),
    }
    other = 0
    allowed = {0x09, 0x0A, 0x0D, 0x1B, 0x07}
    for b in raw:
        if b < 0x20 and b not in allowed:
            other += 1
    counts["other_c0"] = other
    return counts


def normalize(rows: list[str]) -> list[str]:
    """Mask dynamic chrome so two independent runs compare cell-for-cell."""
    out = []
    for line in rows:
        line = line.rstrip()
        line = re.sub(r"v\d+\.\d+\.\d+", "v<ver>", line)
        line = re.sub(r"/private/var/folders/\S+|/var/folders/\S+", "<repo>", line)
        # Elapsed time appears as "· 53s" or "· 1m 20s" between metadata items;
        # it is wall-clock noise between two independent runs, not screen
        # content. Drop the whole token including its leading separator so a
        # run that reports it and one that does not compare equal.
        line = re.sub(r"\s*·\s*(?:\d+m\s+)?\d+s\b", "", line)
        line = re.sub(r"\d{2}:\d{2}", "<clock>", line)
        line = re.sub(r"↓~[\d,]+", "↓~<n>", line)
        line = re.sub(r"\[[%s]\]" % SPINNERS, "⠿", line)
        line = "".join("⠿" if ch in SPINNERS else ch for ch in line)
        out.append(line)
    return out


def frame_audit(rows: list[str]) -> list[str]:
    bad = []
    for row in rows:
        hits = [c for c in row if (c != " " and (ord(c) < 0x20 or c == "\x7f"))]
        if hits:
            bad.append(row)
    return bad


def process_stats(pid: int) -> dict:
    """RSS and cumulative CPU time of the TUI process, read once at the end.

    Not a peak-RSS sampler: it records the steady state after the stream, and
    the report says so. Wall time is measured by the harness.
    """
    try:
        out = subprocess.check_output(
            ["ps", "-o", "rss=,time=", "-p", str(pid)], text=True, timeout=5
        ).strip()
    except Exception:
        return {}
    parts = out.split()
    if len(parts) < 2:
        return {}
    try:
        return {"rss_kb": int(parts[0]), "cpu_time": parts[1]}
    except ValueError:
        return {}


def run_stream(args, text: str, pause_offsets: set[int]):
    """Boot one TUI, stream `text`, snapshot every pause, return snapshots."""
    chunks = chunks_of(text, args.chunk, args.seed)
    script = StreamScript(text, chunks, pause_offsets)
    mock, root, env, pty_ = start_tui(args.bin, args.cols, args.rows)
    mock.turns = [script.frames()]
    mock.stream_delay = args.delay_ms / 1000.0
    mock.stream_hook = script.hook
    snapshots: list[tuple[int, list[str]]] = []
    started = time.time()
    try:
        if not wait_boot(pty_):
            raise RuntimeError("boot failed: " + pty_.screen.text()[-500:])
        submit(pty_, args.prompt)
        seen: int | None = None
        deadline = time.time() + args.timeout
        while time.time() < deadline:
            paused = script.wait_pause(seen, max(0.0, deadline - time.time()))
            if paused is None:
                if script.done():
                    break
                continue
            seen = paused
            # The pause is held until ack; wait only for the paint bytes the
            # delta already triggered.
            pty_.wait_stable(0.25, 10.0)
            rows = pty_.screen.lines()
            snapshots.append((paused, rows))
            script.acknowledge(paused)
        # A single large delta can leave the bytes quiet while the runtime is
        # still parsing it; the quiet screen would still carry the streaming
        # footer. Wait until the turn actually leaves the streaming state, then
        # require byte stability so the last paint is fully flushed.
        settle_deadline = time.time() + args.timeout
        while time.time() < settle_deadline:
            text_now = pty_.screen.text()
            if "等待模型" not in text_now and "Esc 打断" not in text_now:
                break
            time.sleep(0.1)
        pty_.wait_stable(1.0, 30.0)
        final = pty_.screen.lines()
        stats = {
            "wall_s": round(time.time() - started, 2),
            "process": process_stats(pty_.proc.pid),
        }
        raw = bytes(pty_.raw)
    finally:
        pty_.close()
        mock.stop()
        shutil.rmtree(root, ignore_errors=True)
    stats["raw_bytes"] = len(raw)
    stats["full_clears"] = raw.count(b"\x1b[2J")
    stats["tab_bytes"] = raw.count(b"\t")
    return snapshots, final, raw, stats


def probe(args) -> int:
    with open(FIXTURE) as fh:
        text = fh.read()
    if args.prefix:
        text = text[: args.prefix]
    chunks = chunks_of(text, args.chunk, args.seed)
    offsets = offsets_after(chunks)
    if args.pause_every:
        pause_offsets = set(offsets[args.pause_every - 1 :: args.pause_every])
    else:
        pause_offsets = set()
    if args.pause_at:
        pause_offsets |= {int(v) for v in args.pause_at.split(",") if v}
    pause_offsets = {o for o in pause_offsets if 0 < o < len(text)}
    run_dir = os.path.join(
        EVIDENCE, "probe-%dx%d-c%d-%s" % (args.cols, args.rows, args.chunk, args.seed or "seq")
    )
    os.makedirs(run_dir, exist_ok=True)
    snapshots, final, raw, stats = run_stream(args, text, pause_offsets)
    for offset, rows in snapshots:
        dump_frame(os.path.join(run_dir, "frame-%05d.txt" % offset), rows)
    dump_frame(os.path.join(run_dir, "frame-final.txt"), final)
    with open(os.path.join(run_dir, "pty.raw"), "wb") as fh:
        fh.write(raw)

    ctrl = control_bytes(raw)
    audit = []
    for offset, rows in snapshots + [(len(text), final)]:
        for row in frame_audit(rows):
            audit.append([offset, row])
    # Residue is reported only for the frames that are still mid-stream; the
    # final answer legitimately contains those strings.
    residue = []
    for offset, rows in snapshots:
        joined = "\n".join(rows)
        for sig in RESIDUE:
            if sig in joined and sig not in text[:offset]:
                residue.append([offset, sig])
    report = {
        "mode": "probe",
        "size": [args.cols, args.rows],
        "chunk": args.chunk,
        "seed": args.seed,
        "snapshots": len(snapshots),
        "control_bytes": ctrl,
        "frame_control_rows": audit[:20],
        "residue_diagnostics": residue[:20],
        "stats": stats,
        "evidence": run_dir,
    }
    print(json.dumps(report, ensure_ascii=False, indent=2))
    ok = ctrl["tab"] == 0 and ctrl["other_c0"] == 0 and not audit
    return 0 if ok else 1


def equivalence(args) -> int:
    """Incremental mid-stream screen == one-shot paint of the same prefix."""
    with open(FIXTURE) as fh:
        text = fh.read()
    offsets = [int(v) for v in args.pause_at.split(",") if v]
    chunks = chunks_of(text, args.chunk, args.seed)
    boundaries = set(offsets_after(chunks))
    offsets = [o for o in offsets if o in boundaries and 0 < o < len(text)]
    if not offsets:
        print("no offset lands on a chunk boundary")
        return 2
    run_dir = os.path.join(EVIDENCE, "equiv-%dx%d-c%d" % (args.cols, args.rows, args.chunk))
    os.makedirs(run_dir, exist_ok=True)

    # One incremental run, paused at every chosen offset.
    snapshots, _, raw_a, stats_a = run_stream(args, text, set(offsets))
    incremental = dict(snapshots)
    dump_frame(os.path.join(run_dir, "incremental-final.txt"), snapshots[-1][1] if snapshots else [])
    with open(os.path.join(run_dir, "incremental.raw"), "wb") as fh:
        fh.write(raw_a)

    results = []
    failures = 0
    for offset in offsets:
        one_shot, final_b, raw_b, stats_b = run_stream(args, text[:offset], {offset})
        if not one_shot:
            results.append({"offset": offset, "ok": None, "detail": "one-shot run had no snapshot"})
            failures += 1
            continue
        a_rows = incremental.get(offset, [])
        b_rows = one_shot[0][1]
        a = normalize(a_rows)
        b = normalize(b_rows)
        dump_frame(os.path.join(run_dir, "incremental-%05d.txt" % offset), a_rows)
        dump_frame(os.path.join(run_dir, "oneshot-%05d.txt" % offset), b_rows)
        if a == b:
            results.append({"offset": offset, "ok": True})
        else:
            failures += 1
            diff = [
                {"row": i, "incremental": x, "oneshot": y}
                for i, (x, y) in enumerate(zip(a, b))
                if x != y
            ][:12]
            results.append({"offset": offset, "ok": False, "diff": diff})
            dump_frame(
                os.path.join(run_dir, "diff-%05d.txt" % offset),
                [json.dumps(r, ensure_ascii=False) for r in diff],
            )
        ctrl = control_bytes(raw_b)
        if ctrl["tab"] or ctrl["other_c0"]:
            results[-1]["control_bytes"] = ctrl
        results[-1]["oneshot_stats"] = stats_b
    report = {
        "mode": "equivalence",
        "offsets": offsets,
        "results": results,
        "incremental_stats": stats_a,
        "evidence": run_dir,
    }
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if failures == 0 else 1


def matrix(args) -> int:
    """Run the real-PTY matrix: sizes x chunkings, raw byte contract and
    incremental-vs-one-shot equivalence at two offsets plus the final screen."""
    with open(FIXTURE) as fh:
        text = fh.read()
    configs = [
        {"cols": 120, "rows": 40, "chunk": 1, "seed": None},
        {"cols": 120, "rows": 36, "chunk": 7, "seed": 42},
        {"cols": 130, "rows": 36, "chunk": 3, "seed": 7},
        {"cols": 100, "rows": 40, "chunk": 4, "seed": 1},
    ]
    if args.configs:
        configs = []
        for spec in args.configs.split(";"):
            cols, rows, chunk, seed = spec.split(",")
            configs.append(
                {
                    "cols": int(cols),
                    "rows": int(rows),
                    "chunk": int(chunk),
                    "seed": None if seed in ("", "seq") else int(seed),
                }
            )
    report = {"mode": "matrix", "configs": []}
    failures = 0
    for cfg in configs:
        run = argparse.Namespace(**vars(args))
        run.cols, run.rows, run.chunk, run.seed = (
            cfg["cols"],
            cfg["rows"],
            cfg["chunk"],
            cfg["seed"],
        )
        chunks = chunks_of(text, run.chunk, run.seed)
        offsets = offsets_after(chunks)
        pauses = {
            offset
            for index, offset in enumerate(offsets)
            if 1000 <= offset <= 1440 and index % 24 == 0
        }
        if offsets:
            pauses.add(offsets[max(0, len(offsets) - 2)])
        pauses = {o for o in pauses if 0 < o < len(text)}
        name = "matrix-%dx%d-c%d-%s" % (
            run.cols,
            run.rows,
            run.chunk,
            run.seed if run.seed is not None else "seq",
        )
        run_dir = os.path.join(EVIDENCE, name)
        os.makedirs(run_dir, exist_ok=True)
        snapshots, final, raw, stats = run_stream(run, text, pauses)
        for offset, rows in snapshots:
            dump_frame(os.path.join(run_dir, "incremental-%05d.txt" % offset), rows)
        dump_frame(os.path.join(run_dir, "incremental-final.txt"), final)
        with open(os.path.join(run_dir, "incremental.raw"), "wb") as fh:
            fh.write(raw)
        ctrl = control_bytes(raw)
        audit = [
            [offset, row]
            for offset, rows in snapshots + [(len(text), final)]
            for row in frame_audit(rows)
        ]
        boundary = set(offsets)
        equiv_offsets = sorted(
            o for o in pauses if o in boundary and 0 < o < len(text)
        )
        if equiv_offsets:
            pick = {equiv_offsets[0], equiv_offsets[-1]}
            pick.add(min(equiv_offsets, key=lambda o: abs(o - 1220)))
        else:
            pick = set()
        equiv = []
        for offset in sorted(pick):
            oneshot, final_b, raw_b, stats_b = run_stream(run, text[:offset], {offset})
            if not oneshot:
                equiv.append({"offset": offset, "ok": None})
                failures += 1
                continue
            a_rows = dict(snapshots).get(offset, [])
            b_rows = oneshot[0][1]
            dump_frame(os.path.join(run_dir, "oneshot-%05d.txt" % offset), b_rows)
            if normalize(a_rows) == normalize(b_rows):
                equiv.append({"offset": offset, "ok": True})
            else:
                failures += 1
                diff = [
                    {"row": i, "incremental": x, "oneshot": y}
                    for i, (x, y) in enumerate(zip(normalize(a_rows), normalize(b_rows)))
                    if x != y
                ][:10]
                equiv.append({"offset": offset, "ok": False, "diff": diff})
            ctrl_b = control_bytes(raw_b)
            if ctrl_b["tab"] or ctrl_b["other_c0"]:
                equiv[-1]["control_bytes"] = ctrl_b
            equiv[-1]["oneshot_stats"] = stats_b
        # End state: the completed incremental screen against a single-delta
        # paint of the same finished answer. Both sides must be captured after
        # their stream ends; a paused mid-stream frame carries the streaming
        # footer and cannot be compared with a settled screen.
        end_text = text
        end_run = argparse.Namespace(**vars(run))
        end_run.chunk = len(end_text)
        end_run.seed = None
        _, one_shot_final, _, _ = run_stream(end_run, end_text, set())
        end_ok = normalize(final) == normalize(one_shot_final)
        if not end_ok:
            failures += 1
            dump_frame(os.path.join(run_dir, "final-oneshot.txt"), one_shot_final)
        ok = ctrl["tab"] == 0 and ctrl["other_c0"] == 0 and not audit and all(
            e.get("ok") is not False for e in equiv
        ) and end_ok
        if not ok:
            failures += 1
        entry = {
            "config": name,
            "size": [run.cols, run.rows],
            "chunk": run.chunk,
            "seed": run.seed,
            "snapshots": len(snapshots),
            "control_bytes": ctrl,
            "frame_control_rows": audit[:10],
            "equivalence": equiv,
            "final_equals_single_delta": end_ok,
            "stats": stats,
            "ok": ok,
            "evidence": run_dir,
        }
        report["configs"].append(entry)
        print(json.dumps(entry, ensure_ascii=False, indent=2))
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if failures == 0 else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=BIN)
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--chunk", type=int, default=6)
    ap.add_argument("--seed", type=int, default=None)
    ap.add_argument("--delay-ms", type=float, default=6.0)
    ap.add_argument("--prefix", type=int, default=0)
    ap.add_argument("--pause-every", type=int, default=0)
    ap.add_argument("--pause-at", default="")
    ap.add_argument("--prompt", default="现在什么情况")
    ap.add_argument("--timeout", type=float, default=300.0)
    ap.add_argument("--configs", default="")
    ap.add_argument("--mode", choices=["probe", "equivalence", "matrix"], default="probe")
    args = ap.parse_args()
    if args.mode == "probe":
        return probe(args)
    if args.mode == "equivalence":
        return equivalence(args)
    return matrix(args)


if __name__ == "__main__":
    sys.exit(main())
