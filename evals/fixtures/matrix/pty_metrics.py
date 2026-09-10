#!/usr/bin/env python3
"""Agent-runtime performance for a PTY run, taken from the store it left behind.

Three numbers get confused with each other and must not be: how long the model
kept us waiting, how long tools ran, and what the harness itself cost. Only the
third is CodeLeveler's to fix, and it is measured here as the residue — total
wall minus model wait minus tool execution — rather than assumed.

    python3 evals/fixtures/matrix/pty_metrics.py <lab>/sessions/*/sessions.db
"""

import json
import os
import sqlite3
import sys
from datetime import datetime


def parse_ts(text):
    return datetime.fromisoformat(text.replace("Z", "+00:00"))


def facts(db_path):
    """One row per SESSION, not per store.

    A store can hold more than one session — a rehearsal run and the cohort run
    reusing the same isolated home is enough. Summing a whole store then spans
    the idle gap between them, which lands in the harness residue and reads as
    overhead the harness never spent.
    """
    con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        ids = [r[0] for r in con.execute("SELECT id FROM sessions ORDER BY created_at")]
        return [f for f in (_facts(con, db_path, sid) for sid in ids) if f]
    finally:
        con.close()


def _facts(con, db_path, session_id):
    events = list(
        con.execute(
            "SELECT type, payload, created_at FROM events WHERE session_id = ?1 "
            "ORDER BY sequence",
            (session_id,),
        )
    )
    if not events:
        return None
    first, last = parse_ts(events[0][2]), parse_ts(events[-1][2])
    session_span = (last - first).total_seconds()

    # Attribute inside the TURNS, not across the session. A session's first-to-
    # last event also spans the time nobody was doing anything — the gap between
    # reopening a session and typing into it — and charging that to the harness
    # reported two minutes of overhead that nothing spent.
    turn_wall, turn_open = 0.0, None
    for kind, _, created in events:
        if kind == "turn_started":
            turn_open = parse_ts(created)
        elif kind == "turn_finished" and turn_open is not None:
            turn_wall += (parse_ts(created) - turn_open).total_seconds()
            turn_open = None
    wall = turn_wall or session_span

    requests = list(
        con.execute(
            "SELECT latency_ms, input_tokens, output_tokens, cached_input_tokens, "
            "cost_usd_micros, error_kind, retry_count FROM model_requests "
            "WHERE session_id = ?1",
            (session_id,),
        )
    )
    model_wait = sum((r[0] or 0) for r in requests) / 1000.0

    # Tool execution: the gap between a call starting and finishing. Read-only
    # calls can run concurrently, so summing them would exceed the wall clock —
    # merge the intervals instead and report occupied time.
    spans = {}
    intervals = []
    for kind, payload, created in events:
        if kind not in ("tool_call_started", "tool_call_finished"):
            continue
        try:
            call_id = json.loads(payload)["payload"]["call_id"]
        except (KeyError, ValueError):
            continue
        if kind == "tool_call_started":
            spans[call_id] = parse_ts(created)
        elif call_id in spans:
            intervals.append((spans.pop(call_id), parse_ts(created)))
    intervals.sort()
    tool_time, cursor = 0.0, None
    for start, end in intervals:
        if cursor is None or start > cursor:
            tool_time += (end - start).total_seconds()
            cursor = end
        elif end > cursor:
            tool_time += (end - cursor).total_seconds()
            cursor = end

    # Verification runs AFTER the turn's last tool call, so it is inside the
    # wall clock but outside both the model-wait and tool-execution buckets.
    # Leaving it in the residue reported 375 seconds of "harness overhead" for
    # a self-dogfood run whose real cost was a cold `cargo` build.
    verify_seconds, verify_open = 0.0, None
    for kind, _, created in events:
        if kind == "verification_started":
            verify_open = parse_ts(created)
        elif kind == "verification_finished" and verify_open is not None:
            verify_seconds += (parse_ts(created) - verify_open).total_seconds()
            verify_open = None

    # Waiting for a person is not overhead either. An interactive session that
    # asks a clarifying question blocks until it is answered or until the
    # five-minute timeout fires; under an unattended driver that is always the
    # timeout, and 300 seconds of it landed in the residue and read as harness
    # cost on the longest run in the cohort.
    clarify_seconds, clarify_open = 0.0, None
    for kind, _, created in events:
        if kind == "clarification_requested":
            clarify_open = parse_ts(created)
        elif kind == "clarification_answered" and clarify_open is not None:
            clarify_seconds += (parse_ts(created) - clarify_open).total_seconds()
            clarify_open = None

    rounds = 0
    outcome = stop = None
    for kind, payload, _ in events:
        if kind == "turn_finished":
            body = json.loads(payload)["payload"]
            rounds += body.get("rounds") or 0
        elif kind == "task_finished":
            body = json.loads(payload)["payload"]
            outcome, stop = body.get("outcome"), body.get("stop")

    tools = sum(1 for k, _, _ in events if k == "tool_call_started")
    # Verification runs after `turn_finished`, i.e. outside the window `wall`
    # measures, so it is reported beside the attribution rather than inside it.
    # The residue is clamped: a turn window that spans a process kill (the
    # resume case) otherwise reports the dead time as overhead.
    harness = max(0.0, wall - model_wait - tool_time - clarify_seconds)
    return {
        "db": db_path,
        "session": session_id,
        "run": os.path.basename(os.path.dirname(db_path)),
        "wall_seconds": round(wall, 1),
        "session_span_seconds": round(session_span, 1),
        "model_wait_seconds": round(model_wait, 1),
        "tool_seconds": round(tool_time, 1),
        "verification_seconds": round(verify_seconds, 1),
        "clarification_wait_seconds": round(clarify_seconds, 1),
        "harness_seconds": round(harness, 1),
        "model_wait_share": round(model_wait / wall, 3) if wall else None,
        "tool_share": round(tool_time / wall, 3) if wall else None,
        "verification_share": round(verify_seconds / wall, 3) if wall else None,
        "clarification_share": round(clarify_seconds / wall, 3) if wall else None,
        "harness_share": round(harness / wall, 3) if wall else None,
        "rounds": rounds,
        "model_requests": len(requests),
        "model_errors": sum(1 for r in requests if r[5]),
        "retries": sum((r[6] or 0) for r in requests),
        "input_tokens": sum((r[1] or 0) for r in requests),
        "cached_input_tokens": sum((r[3] or 0) for r in requests),
        "output_tokens": sum((r[2] or 0) for r in requests),
        "cost_usd": round(sum((r[4] or 0) for r in requests) / 1e6, 4),
        "tool_calls": tools,
        "events": len(events),
        "outcome": outcome,
        "stop": stop,
    }


def main():
    rows = [row for p in sys.argv[1:] for row in facts(p)]
    rows.sort(key=lambda r: -r["wall_seconds"])
    print(
        f"{'run':<22}{'wall':>8}{'model':>8}{'tool':>8}{'verify':>8}"
        f"{'clarify':>9}{'harness':>9}{'rounds':>8}{'reqs':>6}{'cost$':>9}  outcome"
    )
    for r in rows:
        print(
            f"{r['run'][:22]:<22}{r['wall_seconds']:>8.1f}{r['model_wait_seconds']:>8.1f}"
            f"{r['tool_seconds']:>8.1f}{r['verification_seconds']:>8.1f}"
            f"{r['clarification_wait_seconds']:>9.1f}{r['harness_seconds']:>9.1f}"
            f"{r['rounds']:>8}"
            f"{r['model_requests']:>6}{r['cost_usd']:>9.4f}  {r['outcome']}/{r['stop']}"
        )
    if rows:
        def med(key):
            vals = sorted(r[key] for r in rows if r[key] is not None)
            return vals[len(vals) // 2]

        print(
            f"\nmedian wall {med('wall_seconds')}s · model share {med('model_wait_share')}"
            f" · tool share {med('tool_share')} · verify share {med('verification_share')}"
            f" · clarify share {med('clarification_share')}"
            f" · harness share {med('harness_share')}"
            f" · rounds {med('rounds')}"
        )
        print(f"total cost ${sum(r['cost_usd'] for r in rows):.4f}"
              f" · input {sum(r['input_tokens'] for r in rows):,}"
              f" (cached {sum(r['cached_input_tokens'] for r in rows):,})"
              f" · output {sum(r['output_tokens'] for r in rows):,}")
    out = os.environ.get("METRICS_OUT")
    if out:
        with open(out, "w") as fh:
            json.dump(rows, fh, ensure_ascii=False, indent=2)
        print(f"wrote {out}")


if __name__ == "__main__":
    main()
