"""Natural-spawn extractor.

Ported from the frozen MA-WA1 Gate V2 definition in
`codeleveler-dogfood-control/delegation-metric-audit/scripts/spawn_metric.py`.

`spawn_agent` is a virtual injected tool. The drive loop intercepts it before
the host tool pipeline and does not emit `tool_call_started` for it. Counting
`tool_call_started WHERE name='spawn_agent'` therefore reports zero on runs
that genuinely delegated. The corrected metric counts durable
`sub_agent_started` events whose role is not `reviewer`, once per child id.
"""

from __future__ import annotations

import json
import os
import sqlite3
from typing import Any, Iterator

REVIEWER_ROLE = "reviewer"
MUTATORS = {"apply_patch", "replace", "write_file", "create_file"}


def connect_ro(path: str) -> sqlite3.Connection:
    """Read-only open that survives a checkpointed-WAL database with no -shm.

    `immutable=1` ignores the WAL. That undercounts if a non-empty -wal is
    still on disk. Refuse instead of silently dropping uncheckpointed events.
    """
    uri = f"file:{path}?mode=ro"
    try:
        con = sqlite3.connect(uri, uri=True, timeout=5.0)
        con.execute("select 1 from sqlite_master limit 1")
        return con
    except sqlite3.OperationalError:
        wal = path + "-wal"
        if os.path.exists(wal) and os.path.getsize(wal) > 0:
            raise RuntimeError(
                f"{path}: cannot open read-only and a non-empty -wal is present; "
                "immutable=1 would drop uncheckpointed events. Copy db+wal+shm "
                "to a scratch directory and read the copy."
            ) from None
        return sqlite3.connect(f"file:{path}?mode=ro&immutable=1", uri=True, timeout=5.0)


def event_rows(con: sqlite3.Connection, etype: str) -> Iterator[tuple[int, dict[str, Any]]]:
    cur = con.cursor()
    cur.execute(
        "select sequence, payload from events where type=? order by sequence",
        (etype,),
    )
    for seq, payload in cur.fetchall():
        try:
            yield seq, json.loads(payload).get("payload") or {}
        except (TypeError, ValueError, json.JSONDecodeError):
            continue


def extract_con(con: sqlite3.Connection) -> dict[str, Any]:
    seen: dict[str, dict[str, Any]] = {}
    order: list[str] = []
    duplicate_projections = 0
    for seq, p in event_rows(con, "sub_agent_started"):
        cid = p.get("id")
        if cid is None:
            continue
        if cid in seen:
            duplicate_projections += 1
            continue
        seen[cid] = {
            "id": cid,
            "role": p.get("role"),
            "nickname": p.get("nickname"),
            "sequence": seq,
            "spawned_by_agent_id": p.get("agent_id") or p.get("parent_agent_id"),
            "task": (p.get("task") or "")[:300],
            "profile_id": p.get("profile_id"),
            "profile_role": p.get("profile_role"),
            "capabilities": p.get("capabilities") or [],
        }
        order.append(cid)

    children = [seen[c] for c in order]
    task_children = [c for c in children if c["role"] != REVIEWER_ROLE]
    parent_task_children = [c for c in task_children if not c["spawned_by_agent_id"]]
    child_task_children = [c for c in task_children if c["spawned_by_agent_id"]]

    old_count = 0
    for _seq, p in event_rows(con, "tool_call_started"):
        if (p.get("name") or p.get("tool")) == "spawn_agent":
            old_count += 1

    finished: dict[str, Any] = {}
    finished_seq: dict[str, int] = {}
    for seq, p in event_rows(con, "sub_agent_finished"):
        if p.get("id"):
            finished[p["id"]] = {
                "ok": p.get("ok"),
                "outcome": p.get("outcome"),
                "summary": (p.get("summary") or "")[:300],
                "sequence": seq,
                "contribution": p.get("contribution"),
            }
            finished_seq[p["id"]] = seq

    stages = [
        {
            "sequence": s,
            "action": p.get("action"),
            "detail": (p.get("detail") or "")[:300],
        }
        for s, p in event_rows(con, "delegation_stage")
    ]

    mutations_by_child: dict[str, int] = {}
    parent_tool_calls = 0
    for _seq, p in event_rows(con, "tool_call_started"):
        name = p.get("name") or p.get("tool")
        agent = p.get("agent_id")
        if agent is None:
            parent_tool_calls += 1
        if name in MUTATORS and agent:
            mutations_by_child[agent] = mutations_by_child.get(agent, 0) + 1

    granted = {
        s["detail"].split(":", 1)[0].strip()
        for s in stages
        if s["action"] == "ownership_granted" and ":" in (s["detail"] or "")
    }
    useful = sorted(cid for cid in granted if mutations_by_child.get(cid, 0) > 0)

    first = task_children[0]["sequence"] if task_children else None
    offer_seqs = [s["sequence"] for s in stages if s["action"] in ("offered", "reoffered")]
    min_finish = min(finished_seq.values()) if finished_seq else None

    parent_after = False
    parent_mut_after = False
    resolve_n = 0
    report_ids: list[str] = []
    reads_before = 0
    reads_after = 0
    for seq, p in event_rows(con, "tool_call_started"):
        name = p.get("name") or p.get("tool")
        agent = p.get("agent_id")
        if name == "report_finding" and agent:
            report_ids.append(str(agent))
        if agent is not None:
            continue
        after = min_finish is not None and seq > min_finish
        if after:
            parent_after = True
            if name in MUTATORS:
                parent_mut_after = True
        if name == "resolve_finding":
            resolve_n += 1
        if name == "read_file":
            if after:
                reads_after += 1
            elif first is None or seq < first:
                reads_before += 1

    plan_after = False
    if min_finish is not None:
        for seq, _p in event_rows(con, "plan_updated"):
            if seq > min_finish:
                plan_after = True
                break

    return {
        "natural_spawn_count": len(parent_task_children),
        "child_originated_spawn_count": len(child_task_children),
        "reviewer_children": len(children) - len(task_children),
        "duplicate_child_projections": duplicate_projections,
        "natural_spawn_child_ids": [c["id"] for c in parent_task_children],
        "children": children,
        "first_spawn_sequence": first,
        "old_toolcall_spawn_count": old_count,
        "children_granted_scope": sorted(granted),
        "child_mutations_by_id": mutations_by_child,
        "useful_child_count": len(useful),
        "useful_child_ids": useful,
        "delegation_stages": stages,
        "delegation_stage_actions": [s["action"] for s in stages],
        "offer_sequences": offer_seqs,
        "offer_count": sum(1 for s in stages if s["action"] == "offered"),
        "reoffer_count": sum(1 for s in stages if s["action"] == "reoffered"),
        "parent_tool_calls": parent_tool_calls,
        "sub_agent_outcomes": finished,
        "parent_tool_calls_after_child": parent_after,
        "parent_mutations_after_child": parent_mut_after,
        "parent_resolve_finding_count": resolve_n,
        "child_report_finding_ids": report_ids,
        "parent_reads_before_child": reads_before,
        "parent_reads_after_child": reads_after,
        "plan_updates_after_child": plan_after,
    }


def extract(db_path: str) -> dict[str, Any]:
    con = connect_ro(db_path)
    try:
        return extract_con(con)
    finally:
        con.close()


def fixture_db(events: list[tuple[str, dict[str, Any]]]) -> sqlite3.Connection:
    """In-memory events table for extractor tests. Not used in production scoring."""
    con = sqlite3.connect(":memory:")
    con.execute("create table events (sequence integer, type text, payload text)")
    for i, (etype, payload) in enumerate(events, start=1):
        con.execute(
            "insert into events values (?,?,?)",
            (i, etype, json.dumps({"type": etype, "payload": payload})),
        )
    return con
