#!/usr/bin/env python3
"""Phase C §25: interrupt a real run, resume it, and check what survived.

Materializes a case exactly as the comparative runner does, starts a headless
run, kills the process group mid-flight, then resumes by session id and reads
the durable state on both sides of the break.
"""
from __future__ import annotations

import json
import os
import pathlib
import signal
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time

D = pathlib.Path("/Users/mengmian/Develop/app/dengmengmian/dogfood")
sys.path.insert(0, str(D / "eval" / "scripts"))
import importlib.util

spec = importlib.util.spec_from_file_location(
    "comparative_runner", D / "repos" / "codeleveler" / "evals" / "comparative" / "runner.py")
R = importlib.util.module_from_spec(spec)
spec.loader.exec_module(R)
R.ROOT = D / "repos" / "codeleveler"

CASE = R.ROOT / "evals/cases/navigation/n3-caller-propagation.yaml"
OUT = D / "comparative/runs/beta-phaseC-resume"
BIN = D / "bin/codeleveler/98d21bcd8436/leveler"
KILL_AFTER = int(os.environ.get("KILL_AFTER", "50"))


def open_db(db: pathlib.Path):
    tmp = pathlib.Path(tempfile.mkdtemp()) / db.name
    shutil.copy(db, tmp)
    for suf in ("-wal", "-shm"):
        s = db.with_name(db.name + suf)
        if s.exists():
            shutil.copy(s, tmp.with_name(tmp.name + suf))
    return sqlite3.connect(tmp)


def state(home: pathlib.Path) -> dict:
    for db in sorted(home.rglob("*.db")):
        try:
            con = open_db(db)
        except sqlite3.DatabaseError:
            continue
        cur = con.cursor()
        cur.execute("select name from sqlite_master where type='table' and name='events'")
        if not cur.fetchone():
            con.close()
            continue
        cur.execute("select id, goal, status from sessions")
        sessions = cur.fetchall()
        if not sessions:
            con.close()
            continue
        cur.execute("select type, count(*) from events group by type")
        counts = dict(cur.fetchall())
        cur.execute("select type, payload from events order by sequence")
        plan, tools_started, tools_finished = None, [], []
        for t, p in cur.fetchall():
            b = (json.loads(p).get("payload") or {}) if p else {}
            if t == "plan_updated":
                plan = b
            if t == "tool_call_started":
                tools_started.append(b.get("call_id") or b.get("id"))
            if t in ("tool_call_finished", "tool_call_completed", "tool_call_failed"):
                tools_finished.append(b.get("call_id") or b.get("id"))
        cur.execute("select count(*), sum(input_tokens), sum(output_tokens) from model_requests")
        reqs = cur.fetchone()
        con.close()
        return {
            "db": str(db), "sessions": sessions, "event_counts": counts,
            "requests": reqs[0], "input_tokens": reqs[1], "output_tokens": reqs[2],
            "plan": plan, "tools_started": len(tools_started),
            "tools_finished": len(tools_finished),
            "dangling": [t for t in tools_started if t not in set(tools_finished)],
        }
    return {}


def main():
    case = R.load_cases(["n3-caller-propagation"])[0] if False else __import__("yaml").safe_load(
        CASE.read_text())
    if OUT.exists():
        shutil.rmtree(OUT)
    ws = OUT / "ws"
    R.materialize(case, ws)
    home = OUT / "leveler-home"
    home.mkdir(parents=True, exist_ok=True)
    shutil.copy(D / "config/codeleveler/config.toml", home / "config.toml")
    env = dict(os.environ)
    env["LEVELER_HOME"] = str(home)
    env["RUST_LOG"] = "leveler_agent_core::model_round=info"

    argv = [str(BIN), "run", case["task"], "--repo", str(ws.resolve()),
            "--model", "deepseek/deepseek-v4-flash", "--auto-approve"]
    print(f"[phase-c] starting, will interrupt after {KILL_AFTER}s", flush=True)
    started = time.time()
    proc = subprocess.Popen(argv, cwd=ws, env=env, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, start_new_session=True)
    time.sleep(KILL_AFTER)
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
        killed = True
    else:
        killed = False
    out, _ = proc.communicate()
    (OUT / "run1.log").write_text(R.redact_text(out or ""))
    before = state(home)
    diff1 = subprocess.run(["git", "diff", "--stat", "HEAD"], cwd=ws,
                           capture_output=True, text=True).stdout
    print(json.dumps({
        "phase": "interrupted", "killed_by_us": killed,
        "wall_s": round(time.time() - started, 1),
        "sessions": before.get("sessions"), "requests": before.get("requests"),
        "tools_started": before.get("tools_started"),
        "tools_finished": before.get("tools_finished"),
        "dangling": before.get("dangling"),
        "event_counts": before.get("event_counts"),
        "diff_stat": diff1.strip().splitlines()[-1:] if diff1.strip() else [],
    }, indent=2, ensure_ascii=False), flush=True)

    if not before.get("sessions"):
        print("[phase-c] no durable session; nothing to resume", flush=True)
        return
    sid = before["sessions"][0][0]
    print(f"[phase-c] resuming {sid}", flush=True)
    argv2 = [str(BIN), "run", "--resume", sid, "--repo", str(ws.resolve()), "--auto-approve"]
    p2 = subprocess.Popen(argv2, cwd=ws, env=env, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, text=True, start_new_session=True)
    try:
        out2, _ = p2.communicate(timeout=1200)
        rc2 = p2.returncode
    except subprocess.TimeoutExpired:
        os.killpg(p2.pid, signal.SIGKILL)
        out2, _ = p2.communicate()
        rc2 = -9
    (OUT / "run2.log").write_text(R.redact_text(out2 or ""))
    after = state(home)
    expect_ok, _, tail, _ = R.run_expect(case, ws)
    diff2 = subprocess.run(["git", "diff", "--stat", "HEAD"], cwd=ws,
                           capture_output=True, text=True).stdout
    print(json.dumps({
        "phase": "resumed", "rc": rc2,
        "sessions": after.get("sessions"), "requests": after.get("requests"),
        "tools_started": after.get("tools_started"),
        "tools_finished": after.get("tools_finished"),
        "dangling": after.get("dangling"),
        "event_counts": after.get("event_counts"),
        "same_session_id": after.get("sessions", [[None]])[0][0] == sid,
        "session_count": len(after.get("sessions") or []),
        "acceptance_passed": expect_ok,
        "diff_stat": diff2.strip().splitlines()[-1:] if diff2.strip() else [],
    }, indent=2, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
