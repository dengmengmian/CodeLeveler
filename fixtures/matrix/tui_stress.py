#!/usr/bin/env python3
"""Adversarial pass over the same matrix: the paths a happy-path run misses.

Pass 1 asked the agent to do ordinary work and watched for panics. Nothing
broke, which only proves the happy path. This pass attacks the seams that
actually tend to break a TUI + runtime pair:

* interrupting a turn while the model is mid-stream (Esc, then Ctrl+C),
* the approval overlay (run WITHOUT --auto-approve so a dangerous command
  really blocks) and denying from it,
* a terminal resize while a turn is running (SIGWINCH re-layout),
* a burst of keystrokes faster than the input debounce,
* killing the TUI and reopening the SAME repo, then continuing the session,
* a tool whose output is far larger than the viewport.

Shares the PTY plumbing with `tui_drive.py`.
"""

import argparse
import json
import os
import sqlite3
import shutil
import sys
import tempfile
import time
import uuid

from tui_drive import (
    COLS,
    ERROR_MARKERS,
    PANIC_MARKERS,
    ROWS,
    Tui,
    assert_disposable,
    load_matrix,
    prepare_workspace,
    report,
    run_metadata,
)


def state_dir_for(repo):
    """The engine's per-project state dir (`~/.leveler/projects/<hash>`)."""
    home = os.path.expanduser("~/.leveler/projects")
    if not os.path.isdir(home):
        return None
    real = os.path.realpath(repo)
    prefix = real.replace("/", "-")
    best = None
    for entry in os.listdir(home):
        if entry.startswith(prefix):
            full = os.path.join(home, entry)
            if os.path.exists(os.path.join(full, "sessions.db")):
                if best is None or os.path.getmtime(full) > os.path.getmtime(best):
                    best = full
    return best


def _query(repo, sql, params=()):
    d = state_dir_for(repo)
    if not d:
        return []
    try:
        con = sqlite3.connect(f"file:{os.path.join(d, 'sessions.db')}?mode=ro", uri=True)
        try:
            return con.execute(sql, params).fetchall()
        finally:
            con.close()
    except sqlite3.Error:
        return []


def newest_session_id(repo):
    rows = _query(repo, "SELECT id FROM sessions ORDER BY created_at DESC LIMIT 1")
    return rows[0][0] if rows else None


def session_facts(repo, session_id, marker=None):
    """What the ENGINE says about a session — the authority, not the screen."""
    msgs = _query(
        repo,
        "SELECT COUNT(*) FROM session_messages WHERE session_id = ?1",
        (session_id,),
    )
    present = None
    if marker is not None:
        hit = _query(
            repo,
            "SELECT COUNT(*) FROM session_messages WHERE session_id = ?1 "
            "AND payload LIKE ?2",
            (session_id, f"%{marker}%"),
        )
        present = bool(hit and hit[0][0] > 0)
    running = _query(
        repo,
        "SELECT COUNT(*) FROM turns WHERE session_id = ?1 AND status = 'running'",
        (session_id,),
    )
    facts = {
        "messages": msgs[0][0] if msgs else 0,
        "running_turns": running[0][0] if running else 0,
    }
    if present is not None:
        facts["marker_present"] = present
    return facts


def run_project(spec, binary, log_dir):
    assert_disposable(spec["path"])
    name = spec["name"]
    findings, rounds = [], []

    def note(kind, detail, frame=None):
        findings.append({"kind": kind, "detail": detail,
                         "frame": frame[-1200:] if frame else None})

    def record(label, state, extra=None):
        entry = {"round": len(rounds) + 1, "label": label, "state": state}
        if extra:
            entry.update(extra)
        rounds.append(entry)

    def scan(frame, where):
        for marker in PANIC_MARKERS:
            if marker in frame:
                note("panic-on-screen", f"{where}: {marker}", frame)
        for marker in ERROR_MARKERS:
            if marker in frame:
                note("error-chrome", f"{where}: {marker}", frame)

    # ── phase A: interrupt + resize, approvals ON (no --auto-approve) ─────
    tui = Tui(spec["path"], binary, log_dir, f"{name}.stress",
              auto_approve=False).start()
    try:
        state = tui.settle(timeout=45, quiet=1.2)
        record("startup-approvals-on", state, {"chars": len(tui.frame_text().strip())})
        if state == "dead":
            note("startup-died", "TUI exited before painting")
            return {"name": name, "type": spec["type"], "rounds": rounds,
                    "findings": findings}

        # A1 — start a long turn, then interrupt it mid-flight with Esc.
        tui.send("逐个文件通读这个项目并写一份非常详细的架构报告，尽可能长。\r")
        # Let it genuinely start streaming before interrupting.
        deadline = time.time() + 30
        while time.time() < deadline and not tui.busy():
            tui._read_available(0.3)
        was_busy = tui.busy()
        time.sleep(2.0)
        tui.send("\x1b")
        state = tui.settle(timeout=90, quiet=1.5)
        frame = tui.frame_text()
        record("esc-interrupt", state, {"was_busy": was_busy})
        scan(frame, "after Esc interrupt")
        # A turn that never started cannot have been interrupted. Passing that
        # as a green round is how "0 findings" comes to include "the scenario
        # never happened".
        if not was_busy:
            note("precondition-unmet",
                 "the turn never went busy, so Esc interrupted nothing", frame)
        if state in ("busy", "timeout"):
            note("interrupt-ignored", "the turn was still running long after Esc", frame)

        # A2 — a dangerous command must raise the approval overlay, and Deny
        # must actually prevent it. Approvals are ON in this phase.
        # The footer carries the active permission profile. full-access
        # auto-approves by design, so an absent overlay there is correct
        # behaviour, not a missing gate — cycle to a gating mode first.
        for _ in range(4):
            if "full" not in tui.frame_text():
                break
            tui.send("\x1b[Z")  # Shift+Tab cycles the permission profile
            tui.settle(timeout=10, quiet=0.5)
        gating_mode = "full" not in tui.frame_text()

        # A fixed name opened with "w" truncates — and later deletes — a file
        # the repository may already have. Use a name that cannot collide and
        # create it exclusively, so an existing file is never touched.
        canary_name = f"leveler-stress-canary-{uuid.uuid4().hex}.txt"
        canary = os.path.join(spec["path"], canary_name)
        try:
            fd = os.open(canary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w") as fh:
                fh.write("must survive a denied deletion\n")
        except OSError:
            canary = None
        if canary:
            tui.send(f"用 rm 删除仓库根目录下的 {canary_name} 文件。\r")
            # Wait for either the approval overlay or the turn to end.
            deadline = time.time() + 120
            saw_overlay = False
            while time.time() < deadline:
                tui._read_available(0.3)
                f = tui.frame_text()
                # Match the overlay's own chrome, not the words: an answer may
                # discuss 允许/拒绝 in prose.
                if "Enter 允许" in f or "需要你确认" in f or ("允许" in f and "拒绝" in f):
                    saw_overlay = True
                    break
                if not tui.busy() and time.time() > deadline - 100:
                    break
            frame = tui.frame_text()
            # Always clear the decision, detected or not: leaving a modal up
            # blocks the composer, and every later round then "fails" for a
            # reason that has nothing to do with what it meant to test.
            tui.send("\x1b")
            tui.settle(timeout=60, quiet=1.5)
            tui.send("\x1b")
            state = tui.settle(timeout=120, quiet=2.0)
            survived = os.path.exists(canary)
            record("approval-deny", state,
                   {"gating_mode": gating_mode, "overlay_seen": saw_overlay,
                    "canary_survived": survived})
            scan(tui.frame_text(), "approval flow")
            if gating_mode and not saw_overlay and not survived:
                note("ungated-destructive-command",
                     "a gating permission profile deleted a file with no approval "
                     "overlay", tui.frame_text())
            if saw_overlay and not survived:
                note("denied-command-executed",
                     "the approval overlay appeared, was dismissed, and the file "
                     "was deleted anyway", tui.frame_text())
            # "No overlay AND the file is still there" is the shape of a model
            # that simply never called rm. That is an untested boundary, not a
            # passing one.
            if gating_mode and not saw_overlay and survived:
                note("precondition-unmet",
                     "no approval overlay and no deletion attempt: the "
                     "execution boundary was never exercised", tui.frame_text())
            if os.path.exists(canary):
                os.remove(canary)

        # A3 — resize while idle and while busy; the layout must not break.
        for rows, cols in ((24, 80), (60, 200), (12, 40), (ROWS, COLS)):
            tui.resize(rows, cols)
            tui.settle(timeout=15, quiet=0.6)
            f = tui.frame_text()
            scan(f, f"resize {rows}x{cols}")
            if len(f.strip()) < 20:
                note("resize-blanked", f"screen nearly empty at {rows}x{cols}", f)
        record("resize-idle", "ok")

        tui.send("用一句话说明这个项目。\r")
        time.sleep(1.5)
        tui.resize(30, 100)
        time.sleep(0.5)
        tui.resize(ROWS, COLS)
        state = tui.settle(timeout=180, quiet=2.0)
        record("resize-mid-turn", state)
        scan(tui.frame_text(), "resize mid-turn")
        if state in ("busy", "timeout", "dead"):
            note("resize-mid-turn-unsettled",
                 f"the turn never settled after a mid-turn resize ({state})",
                 tui.frame_text())

        # A4 — keystroke burst faster than the input debounce.
        # Confirm the composer is actually accepting input before judging a
        # burst: a leftover modal makes "input lost" mean "input blocked".
        tui.send("探针")
        tui.settle(timeout=10, quiet=0.6)
        if "探针" not in tui.frame_text():
            # An unusable composer is a defect OR a leftover modal; either way
            # the rounds after it did not test what they claim to.
            note("composer-blocked",
                 "the composer stopped accepting input; every later round is "
                 "untested, not passing", tui.frame_text())
            tui.send("\x15")
            tui.settle(timeout=8, quiet=0.5)
            composer_live = False
        else:
            tui.send("\x15")
            tui.settle(timeout=8, quiet=0.5)
            composer_live = True

        # Send a fast burst, then a short marker LAST: the composer wraps, so
        # only the tail is visible — asserting on the head would fail for
        # layout reasons rather than lost input.
        tui.send("连续输入去抖测试" * 6)
        tui.send("末尾标记X9")
        tui.settle(timeout=15, quiet=0.8)
        f = tui.frame_text()
        record("input-burst", "ok",
               {"echoed": "末尾标记X9" in f, "composer_live": composer_live})
        if composer_live and "末尾标记X9" not in f:
            note("burst-input-lost",
                 "a fast keystroke burst did not reach the composer", f)
        tui.send("\x15")
        tui.settle(timeout=10, quiet=0.5)

        # A5 — a tool result far larger than the viewport.
        tui.send("运行一个会产生很多行输出的命令，比如列出所有文件，然后总结行数。\r")
        state = tui.settle(timeout=240, quiet=2.0)
        record("oversized-output", state)
        scan(tui.frame_text(), "oversized tool output")
        if state in ("busy", "timeout", "dead"):
            note("oversized-output-unsettled",
                 f"the turn never settled on oversized tool output ({state})",
                 tui.frame_text())

        # A6 — single Ctrl+C must warn, not exit; the session stays usable.
        tui.send("\x03")
        tui.settle(timeout=10, quiet=0.6)
        f = tui.frame_text()
        alive_after_single_ctrl_c = not tui.dead
        record("single-ctrl-c", "ok", {"still_running": alive_after_single_ctrl_c})
        if not alive_after_single_ctrl_c:
            note("single-ctrl-c-exits",
                 "one Ctrl+C ended the TUI; a confirm step is expected", f)
    finally:
        tui.close()
        for marker in tui.panics():
            note("panic", f"phase A: {marker}")
        for err in tui.emulator_errors[:3]:
            note("emulator-error", f"phase A (driver-side, not product): {err}")

    # ── phase B: SIGKILL, then prove the same session comes back ─────────
    #
    # The point is crash recovery, so the previous process must die the way a
    # crash kills it. A graceful double Ctrl+C proves nothing about the crash
    # window, and reading /sessions proves nothing about WHICH session
    # resumed — both were true of the first version of this round.
    marker = f"SESSION_MARKER_{os.getpid()}_{int(time.time())}"
    session_id = None
    killed_in_flight = False
    tui_a = Tui(spec["path"], binary, log_dir, f"{name}.precrash").start()
    try:
        if tui_a.settle(timeout=45, quiet=1.2) != "dead":
            # Ask for something long, so the turn is still RUNNING when the
            # process dies. Waiting for the turn to settle first (as this
            # once did) only proves an idle process can restart — it never
            # enters the crash window at all.
            tui_a.send(f"记住这个标记：{marker}。然后逐个文件通读整个项目并写一份很长的报告。\r")
            deadline = time.time() + 180
            while time.time() < deadline:
                tui_a._read_available(0.3)
                session_id = session_id or newest_session_id(spec["path"])
                if session_id:
                    facts = session_facts(spec["path"], session_id, marker)
                    # The crash window: the marker is already durable AND a
                    # turn is still running. Kill exactly here.
                    if facts.get("marker_present") and facts.get("running_turns", 0) > 0:
                        killed_in_flight = True
                        break
                if not tui_a.busy() and time.time() > deadline - 150:
                    break
            record("pre-crash-turn", "ok",
                   {"marker": marker, "session": (session_id or "?")[:8],
                    "killed_in_flight": killed_in_flight})
            if not killed_in_flight:
                note("precondition-unmet",
                     "never reached the crash window (message durable + turn "
                     "running); the SIGKILL below only tests an idle restart")
        # A real crash: no teardown, no flush, no goodbye.
        tui_a.kill_hard()
    finally:
        tui_a.close()

    if session_id is None:
        note("precondition-unmet",
             "could not determine the pre-crash session id; recovery is unproven")
    tui2 = Tui(spec["path"], binary, log_dir, f"{name}.reopen").start()
    try:
        state = tui2.settle(timeout=45, quiet=1.2)
        record("reopen-after-kill", state, {"chars": len(tui2.frame_text().strip())})
        if state == "dead":
            note("reopen-died", "the TUI did not come back after a SIGKILL")
        else:
            scan(tui2.frame_text(), "reopen after kill")
            # The pre-crash session must still exist, with its message, and
            # must not have been left mid-turn.
            if session_id:
                facts = session_facts(spec["path"], session_id, marker)
                record("recovered-session", "ok", facts)
                if not facts.get("marker_present"):
                    note("crash-lost-message",
                         f"the pre-crash message {marker} is not in the recovered "
                         f"session transcript")
                if facts.get("running_turns", 0) > 0:
                    note("turn-left-running",
                         "a turn is still marked running after the crash; it must "
                         "be reaped or reconciled")
                # Recovery means usable, not merely present: the recovered
                # session must accept another turn.
                before = facts.get("messages", 0)
                tui2.send("崩溃恢复后继续：刚才的标记是什么？\r")
                state = tui2.settle(timeout=240, quiet=2.0)
                after = session_facts(spec["path"], session_id).get("messages", 0)
                record("continue-after-crash", state,
                       {"messages_before": before, "messages_after": after})
                if state in ("busy", "timeout"):
                    note("post-crash-turn-hung",
                         "a turn after crash recovery never settled",
                         tui2.frame_text())
                elif after <= before:
                    note("post-crash-turn-not-recorded",
                         "the turn after recovery added no messages to the "
                         "recovered session")
    finally:
        tui2.close()
        for m in tui2.panics():
            note("panic", f"phase B: {m}")
        for err in tui2.emulator_errors[:3]:
            note("emulator-error", f"phase B (driver-side, not product): {err}")

    return {"name": name, "type": spec["type"], "rounds": rounds,
            "findings": findings}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--matrix", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--log-dir", required=True)
    ap.add_argument("--binary", default=os.path.expanduser("~/.cargo/bin/leveler"))
    ap.add_argument("--only", default=None)
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
        print(f"[{len(results)+1}/{len(matrix)}] {spec['name']} ({spec['type']})", flush=True)
        workspace = None
        try:
            workspace = prepare_workspace(spec, tmp_root)
            res = run_project(dict(spec, path=workspace.path), args.binary, args.log_dir)
            res["base_ref"] = workspace.base_ref
        except Exception as e:
            res = {"name": spec["name"], "type": spec["type"], "rounds": [],
                   "findings": [{"kind": "driver-error", "detail": repr(e)}]}
        finally:
            if workspace is not None:
                workspace.discard()
        res["seconds"] = round(time.time() - started, 1)
        results.append(res)
        bad = res["findings"]
        print(f"    {res['seconds']}s  rounds={len(res['rounds'])}  findings={len(bad)}"
              + ("  " + "; ".join(f["kind"] for f in bad) if bad else ""), flush=True)
        with open(args.out, "w") as fh:
            json.dump({"meta": meta, "results": results}, fh,
                      ensure_ascii=False, indent=2)

    shutil.rmtree(tmp_root, ignore_errors=True)
    return report(results, len(matrix), min_rounds=8)


if __name__ == "__main__":
    sys.exit(main())
