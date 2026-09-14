"""Child lifecycle truth read from the durable event log. Observer only.

Every number here is a fact the runtime recorded, read back as written:
typed terminals (MA1 `outcome` / `stop`), interruptions and resumes, and the
write scope a child was admitted with. Nothing re-derives a status from
prose, and nothing here decides whether delegation was a good idea.
"""

from __future__ import annotations

import json
import re
import sqlite3
from typing import Any

from spawn_metric import MUTATORS, event_rows

COMPLETED_OUTCOMES = ("completed_with_findings", "completed_no_findings")
# v0.2.0-beta.2 recorded no spawn spec; it appended the admitted files to the
# child's task as a last line instead.
LEGACY_SCOPE = re.compile(r"\n\[scope: ([^\]]*)\]\s*$")
PATCH_HEADER = re.compile(r"^\*\*\* (?:Add|Update|Delete) File: (.+)$|^\*\*\* Move to: (.+)$", re.M)


def _norm(path: str) -> str:
    path = path.strip()
    while path.startswith("./"):
        path = path[2:]
    return path.rstrip("/")


def _in_scope(path: str, scope: set[str]) -> bool:
    return any(path == s or path.startswith(s + "/") for s in scope)


def mutation_paths(name: str, arguments: Any) -> list[str]:
    """Paths a mutating call names. Empty when they cannot be read."""
    try:
        args = json.loads(arguments) if isinstance(arguments, str) else (arguments or {})
    except (TypeError, ValueError):
        return []
    if not isinstance(args, dict):
        return []
    if name == "apply_patch":
        text = args.get("patch") or args.get("input") or ""
        return [_norm(a or b) for a, b in PATCH_HEADER.findall(str(text))]
    path = args.get("path") or args.get("file_path")
    return [_norm(str(path))] if path else []


def _bump(counts: dict[str, int], key: str) -> None:
    counts[key] = counts.get(key, 0) + 1


def child_lifecycle(con: sqlite3.Connection) -> dict[str, Any]:
    started: dict[str, dict[str, Any]] = {}
    for _seq, p in event_rows(con, "sub_agent_started"):
        cid = p.get("id")
        if cid is None or cid in started:
            continue
        spec = p.get("spec")
        if isinstance(spec, dict):
            files = [f for f in (spec.get("files") or []) if isinstance(f, str)]
        else:
            marker = LEGACY_SCOPE.search(str(p.get("task") or ""))
            files = [f for f in marker.group(1).split(",") if f.strip()] if marker else []
        started[cid] = {
            "role": p.get("role"),
            "read_only": p.get("read_only"),
            "scope": {_norm(f) for f in files},
        }

    terminals: dict[str, list[dict[str, Any]]] = {}
    finished_at: dict[str, int] = {}
    for seq, p in event_rows(con, "sub_agent_finished"):
        cid = p.get("id")
        if cid is None:
            continue
        terminals.setdefault(cid, []).append(p)
        finished_at.setdefault(cid, seq)

    by_outcome: dict[str, int] = {}
    by_stop: dict[str, int] = {}
    success = partial = lost = untyped = 0
    for cid, rows in terminals.items():
        first = rows[0]
        outcome, stop = first.get("outcome"), first.get("stop")
        if outcome is None:
            untyped += 1
            _bump(by_outcome, "untyped")
        else:
            _bump(by_outcome, outcome)
        if stop is not None:
            _bump(by_stop, stop)
        if outcome in COMPLETED_OUTCOMES and stop == "completed":
            success += 1
        if outcome == "incomplete_partial":
            partial += 1
        if stop == "lost":
            lost += 1

    interrupted = sum(1 for _ in event_rows(con, "sub_agent_interrupted"))
    resumed = sum(1 for _ in event_rows(con, "sub_agent_resumed"))

    for _seq, p in event_rows(con, "delegation_stage"):
        if p.get("action") != "ownership_granted":
            continue
        owner, _, paths = str(p.get("detail") or "").partition(":")
        child = started.get(owner.strip())
        if child is not None:
            child["scope"].update(_norm(x) for x in paths.split(",") if x.strip())

    # Keyed by (agent, call id): a provider that numbers calls per session
    # can reuse an id across the parent and a child.
    calls: dict[tuple[Any, str], tuple[str, Any, Any, int]] = {}
    for seq, p in event_rows(con, "tool_call_started"):
        if p.get("call_id"):
            calls[(p.get("agent_id"), p["call_id"])] = (p.get("name") or "", p.get("arguments"), p.get("agent_id"), seq)

    mutations: dict[str, list[str]] = {}
    violations: list[dict[str, str]] = []
    unattributed = 0
    for _seq, p in event_rows(con, "tool_call_finished"):
        agent = p.get("agent_id")
        name = p.get("name") or ""
        if agent is None or agent not in started or name not in MUTATORS or p.get("is_error"):
            continue
        call = calls.get((agent, p.get("call_id") or ""))
        paths = mutation_paths(name, call[1]) if call else []
        if not paths:
            unattributed += 1
            continue
        child = started[agent]
        for path in paths:
            mutations.setdefault(agent, []).append(path)
            if child["read_only"] or not _in_scope(path, child["scope"]):
                violations.append({"child": agent, "path": path})

    # Distinct paths per child, and which of them the parent read again after
    # that child settled.
    child_reads: dict[str, set[str]] = {cid: set() for cid in started}
    parent_reads_at: list[tuple[int, str]] = []
    for name, arguments, agent, seq in calls.values():
        if name != "read_file":
            continue
        paths = mutation_paths(name, arguments)
        if agent in started:
            child_reads[agent].update(paths)
        elif agent is None:
            parent_reads_at.extend((seq, path) for path in paths)
    parent_rereads = {
        cid: sorted({path for seq, path in parent_reads_at
                     if cid in finished_at and seq > finished_at[cid] and path in child_reads[cid]})
        for cid in started
    }

    return {
        "children": len(started),
        "by_outcome": by_outcome,
        "by_stop": by_stop,
        "child_success": success,
        "child_partial": partial,
        "untyped_terminals": untyped,
        "lost_accepted_child": lost,
        "duplicate_settlement": sum(1 for rows in terminals.values() if len(rows) > 1),
        "open_orphan": sum(1 for cid in started if cid not in terminals),
        "interrupted": interrupted,
        "resumed": resumed,
        "ownership_violation": len(violations),
        "ownership_violations": violations,
        "unattributed_child_mutations": unattributed,
        "child_mutations": mutations,
        "child_reads": {cid: sorted(paths) for cid, paths in child_reads.items()},
        "parent_rereads": parent_rereads,
    }


def _columns(con: sqlite3.Connection) -> set[str]:
    return {row[1] for row in con.execute("pragma table_info(model_requests)")}


def _lane(rows: list[tuple], has_cached: bool, has_cost: bool) -> dict[str, Any]:
    unpriced = sum(1 for r in rows if r[3] is None) if has_cost else None
    return {
        "requests": len(rows),
        "input_tokens": sum(int(r[0] or 0) for r in rows),
        "output_tokens": sum(int(r[1] or 0) for r in rows),
        "cached_input_tokens": sum(int(r[2] or 0) for r in rows) if has_cached else None,
        # One unpriced request makes the lane's cost unknown: a partial sum
        # would read as a cheaper run.
        "cost_usd_micros": sum(int(r[3]) for r in rows) if has_cost and not unpriced else None,
        "unpriced_requests": unpriced,
    }


def request_usage(con: sqlite3.Connection) -> dict[str, Any]:
    """Requests, tokens, cached tokens and cost, total and per lane."""
    cols = _columns(con)
    if not cols:
        return {"total": None, "parent": None, "child": None}
    has_cached = "cached_input_tokens" in cols
    has_cost = "cost_usd_micros" in cols
    has_agent = "agent_id" in cols
    select = ", ".join([
        "input_tokens", "output_tokens",
        "cached_input_tokens" if has_cached else "null",
        "cost_usd_micros" if has_cost else "null",
        "agent_id" if has_agent else "null",
    ])
    rows = con.execute(f"select {select} from model_requests").fetchall()
    out: dict[str, Any] = {"total": _lane(rows, has_cached, has_cost)}
    if has_agent:
        out["parent"] = _lane([r for r in rows if r[4] is None], has_cached, has_cost)
        out["child"] = _lane([r for r in rows if r[4] is not None], has_cached, has_cost)
    else:
        out["parent"] = out["child"] = None
    return out
