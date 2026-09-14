"""Where a delegated run's wall clock went. Observer only.

Timestamps are the event log's `created_at` and each model request's end time
and latency. The numbers are approximations meant to answer "why was the
multi-agent run faster or slower", not a profiler: planning is the latency of
the parent request that produced the first spawn, a child's runtime runs from
its start to its last recorded work, and coordination overhead is planning +
waiting for settlement + the parent's work after the last settlement.
"""

from __future__ import annotations

import json
import re
import sqlite3
from datetime import datetime
from typing import Any

from child_lifecycle import mutation_paths
from spawn_metric import MUTATORS

REVIEWER = "reviewer"
READERS = {"read_file", "read_symbol", "grep", "find_files", "list_files", "find_symbol", "find_references"}


def _ts(text: str) -> float:
    return datetime.fromisoformat(str(text).replace("Z", "+00:00")).timestamp()


def _r(x: float | None) -> float | None:
    return None if x is None else round(x, 3)


def coordination(con: sqlite3.Connection) -> dict[str, Any]:
    events = []
    for seq, etype, payload, created in con.execute(
        "select sequence, type, payload, created_at from events order by sequence"
    ):
        try:
            body = json.loads(payload).get("payload") or {}
        except (TypeError, ValueError):
            body = {}
        events.append((seq, etype, body, _ts(created)))
    if not events:
        return {}
    t0 = events[0][3]
    finished = [t for _s, e, _b, t in events if e == "task_finished"]
    t_end = finished[-1] if finished else events[-1][3]

    requests = []
    try:
        for created, latency, agent, tin, tout in con.execute(
            "select created_at, latency_ms, agent_id, input_tokens, output_tokens from model_requests"
        ):
            requests.append((_ts(created), (latency or 0) / 1000.0, agent, tin or 0, tout or 0))
    except sqlite3.OperationalError:
        pass

    starts: dict[str, float] = {}
    start_counts: dict[str, int] = {}
    roles: dict[str, str] = {}
    briefs: dict[str, int | None] = {}
    settled: dict[str, float] = {}
    terminal_seq: dict[str, int] = {}
    calls: dict[tuple[Any, str], tuple[str, Any]] = {}
    child_work_end: dict[str, float] = {}
    child_work_start: dict[str, float] = {}
    child_paths: dict[str, set[str]] = {}
    parent_writes: list[tuple[float, str]] = []
    first_parent_mutation = None
    first_parent_read = None
    write_after_terminal = 0

    for seq, etype, body, t in events:
        cid = body.get("id")
        agent = body.get("agent_id")
        if etype == "sub_agent_started" and cid:
            start_counts[cid] = start_counts.get(cid, 0) + 1
            roles.setdefault(cid, body.get("role") or "")
            briefs.setdefault(cid, len(body["task"]) if isinstance(body.get("task"), str) else None)
            starts.setdefault(cid, t)
        elif etype == "sub_agent_finished" and cid and cid not in settled:
            settled[cid] = t
            terminal_seq[cid] = seq
        elif etype == "tool_call_started" and body.get("call_id"):
            name = body.get("name") or ""
            calls[(agent, body["call_id"])] = (name, body.get("arguments"))
            if agent is not None:
                child_work_start[agent] = min(child_work_start.get(agent, t), t)
            if agent is None and name in MUTATORS and first_parent_mutation is None:
                first_parent_mutation = t
            if agent is None and name in READERS and first_parent_read is None:
                first_parent_read = t
        elif etype == "tool_call_finished":
            name, arguments = calls.get((agent, body.get("call_id") or ""), (body.get("name") or "", None))
            if agent is not None:
                child_work_end[agent] = max(child_work_end.get(agent, t), t)
            if name not in MUTATORS or body.get("is_error"):
                continue
            paths = mutation_paths(name, arguments)
            if agent is None:
                parent_writes.extend((t, p) for p in paths)
            else:
                child_paths.setdefault(agent, set()).update(paths)
                if agent in terminal_seq and seq > terminal_seq[agent]:
                    write_after_terminal += 1

    for end, latency, agent, _tin, _tout in requests:
        if agent is not None:
            child_work_end[agent] = max(child_work_end.get(agent, end), end)
            child_work_start[agent] = min(child_work_start.get(agent, end - latency), end - latency)

    task_children = [cid for cid in starts if roles.get(cid) != REVIEWER]
    first_spawn = min((starts[c] for c in task_children), default=None)
    useful_moments = [x for x in (first_parent_mutation, first_spawn) if x is not None]

    planning = None
    pre_spawn = None
    if first_spawn is not None:
        before = [r for r in requests if r[2] is None and r[0] <= first_spawn]
        if before:
            planning = max(before)[1]
        pre_spawn = {
            "requests": len(before),
            "input_tokens": sum(r[3] for r in before),
            "output_tokens": sum(r[4] for r in before),
            "model_wait_s": _r(sum(r[1] for r in before)),
        }

    children = []
    for cid in task_children:
        end = child_work_end.get(cid, settled.get(cid, starts[cid]))
        # Work begins at the child's first request or tool call; the time
        # before that is spent waiting for a concurrency slot.
        work_start = max(starts[cid], child_work_start.get(cid, starts[cid]))
        children.append({
            "id": cid,
            "role": roles.get(cid),
            "start_s": _r(starts[cid] - t0),
            "queue_s": _r(work_start - starts[cid]),
            "runtime_s": _r(end - work_start),
            "settled_s": _r(settled[cid] - t0) if cid in settled else None,
            "brief_chars": briefs.get(cid),
        })

    out: dict[str, Any] = {
        "wall_s": _r(t_end - t0),
        "time_to_first_useful_action_s": _r(min(useful_moments) - t0) if useful_moments else None,
        "time_to_first_spawn_s": _r(first_spawn - t0) if first_spawn is not None else None,
        "parent_planning_s": _r(planning),
        "time_to_first_read_s": _r(first_parent_read - t0) if first_parent_read is not None else None,
        "parent_pre_spawn": pre_spawn,
        "parent_requests": sum(1 for r in requests if r[2] is None),
        "children": children,
        "child_write_after_terminal": write_after_terminal,
        "recovery_duplication": sum(1 for n in start_counts.values() if n > 1),
        "parent_rewrites": {
            cid: sorted({p for t, p in parent_writes
                         if cid in settled and t > settled[cid] and p in child_paths.get(cid, set())})
            for cid in task_children
        },
        "child_critical_path_s": None,
        "serial_child_work_s": None,
        "parallel_time_saved_s": None,
        "settlement_wait_s": None,
        "parent_integration_s": None,
        "coordination_overhead_s": None,
    }
    if children:
        ends = [child_work_end.get(c, settled.get(c, starts[c])) for c in task_children]
        critical = max(ends) - min(starts[c] for c in task_children)
        serial = sum(ch["runtime_s"] for ch in children)
        out["child_critical_path_s"] = _r(critical)
        out["serial_child_work_s"] = _r(serial)
        out["parallel_time_saved_s"] = _r(serial - critical)
        if all(c in settled for c in task_children):
            last_settled = max(settled[c] for c in task_children)
            wait = max(0.0, last_settled - max(ends))
            integration = t_end - last_settled
            out["settlement_wait_s"] = _r(wait)
            out["parent_integration_s"] = _r(integration)
            if planning is not None:
                out["coordination_overhead_s"] = _r(planning + wait + integration)
    return out


ANSI = re.compile(r"\x1b\[[0-9;]*m")
ROUND_STARTED = re.compile(r"model round started request_id=(\S+).*?reasoning_effort=(?:Some\((\w+)\)|(None))")


def reasoning_effort_by_lane(err_text: str, con: sqlite3.Connection) -> dict[str, dict[str, int]]:
    """Effort each model request was sent with, per lane: the product's
    `model round started` trace joined to `model_requests` by request id.
    A traced request with no durable row is `unmatched`, never guessed."""
    lanes = {rid: ("parent" if agent is None else "child")
             for rid, agent in con.execute("select provider_request_id, agent_id from model_requests")}
    out: dict[str, dict[str, int]] = {}
    for line in ANSI.sub("", err_text).splitlines():
        m = ROUND_STARTED.search(line)
        if not m:
            continue
        lane = lanes.get(m.group(1), "unmatched")
        effort = m.group(2) or m.group(3)
        out.setdefault(lane, {})
        out[lane][effort] = out[lane].get(effort, 0) + 1
    return out
