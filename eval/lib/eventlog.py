"""EventLog observation: rounds, edits, plans, safety counters.

Reads durable engine events. Does not reimplement spawn/claim/ownership
admission — those facts are taken from `delegation_stage` and
`sub_agent_started` as the runtime recorded them.
"""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from typing import Any

from spawn_metric import MUTATORS, connect_ro, extract_con

EDIT_TOOLS = {"apply_patch", "replace"}
CLAIM_TOOL = "claim_write_scope"


def load_events(con: sqlite3.Connection) -> list[tuple[int, str, dict[str, Any]]]:
    cur = con.cursor()
    cur.execute("select sequence, type, payload from events order by sequence")
    out: list[tuple[int, str, dict[str, Any]]] = []
    for seq, etype, payload in cur.fetchall():
        try:
            body = json.loads(payload).get("payload") or {}
        except (TypeError, ValueError, json.JSONDecodeError):
            body = {}
        out.append((seq, etype, body))
    return out


def round_index(events: list[tuple[int, str, dict[str, Any]]]) -> dict[int, int]:
    """Map sequence → 1-based round. A `context_snapshot` closes a round."""
    round_at: dict[int, int] = {}
    current = 1
    for seq, etype, _body in events:
        round_at[seq] = current
        if etype == "context_snapshot":
            current += 1
    return round_at


def extract_timeline(con: sqlite3.Connection) -> dict[str, Any]:
    events = load_events(con)
    round_at = round_index(events)
    total_rounds = max(round_at.values(), default=0)

    plan_updates = 0
    parent_edits = 0
    parent_edit_finished = 0
    first_edit_round: int | None = None
    first_plan_round: int | None = None
    claim_calls = 0
    parent_mutations = 0
    tool_names: list[str] = []

    for seq, etype, body in events:
        rnd = round_at.get(seq)
        if etype == "plan_updated":
            plan_updates += 1
            if first_plan_round is None:
                first_plan_round = rnd
        if etype == "tool_call_started":
            name = body.get("name") or body.get("tool") or ""
            agent = body.get("agent_id")
            if agent is None:
                tool_names.append(name)
                if name in MUTATORS:
                    parent_mutations += 1
                    parent_edits += 1 if name in EDIT_TOOLS or name in MUTATORS else 0
                if name == CLAIM_TOOL:
                    claim_calls += 1
        if etype == "tool_call_finished":
            name = body.get("name") or ""
            agent = body.get("agent_id")
            if agent is None and name in EDIT_TOOLS and not body.get("is_error"):
                parent_edit_finished += 1
                if first_edit_round is None:
                    first_edit_round = rnd

    # Fallback: if no successful finished edit was observed, still stamp the
    # first mutating parent start so timing comparisons stay defined.
    if first_edit_round is None:
        for seq, etype, body in events:
            if etype != "tool_call_started":
                continue
            name = body.get("name") or body.get("tool") or ""
            if body.get("agent_id") is None and name in EDIT_TOOLS:
                first_edit_round = round_at.get(seq)
                break

    spawn = extract_con(con)
    stages = spawn["delegation_stages"]
    offer_round: int | None = None
    offer_trigger: str | None = None
    reoffer_round: int | None = None
    kept_round: int | None = None
    delegated_round: int | None = None
    for stage in stages:
        action = stage["action"]
        rnd = round_at.get(stage["sequence"])
        if action == "offered" and offer_round is None:
            offer_round = rnd
            offer_trigger = stage.get("detail") or None
        elif action == "reoffered" and reoffer_round is None:
            reoffer_round = rnd
        elif action == "kept" and kept_round is None:
            kept_round = rnd
        elif action == "delegated" and delegated_round is None:
            delegated_round = rnd

    actions = spawn["delegation_stage_actions"]
    spawned = spawn["natural_spawn_count"] > 0
    kept = "kept" in actions
    delegated = "delegated" in actions or spawned
    delayed_spawn_after_keep = bool(kept and delegated and (delegated_round or 0) > (kept_round or 0))

    if delegated:
        disposition = "delegated"
    elif kept:
        disposition = "kept"
    elif "offered" in actions:
        disposition = "offered_only"
    else:
        disposition = "none"

    engaged = plan_updates >= 1 or parent_mutations >= 1
    goal = None
    model = None
    for _seq, etype, body in events:
        if etype == "task_started":
            goal = body.get("goal")
            model = body.get("model")
            break

    ownership_granted = sum(1 for a in actions if a == "ownership_granted")
    ownership_denied = sum(1 for a in actions if a == "ownership_denied")
    first_spawn_seq = spawn.get("first_spawn_sequence")
    first_spawn_round = round_at.get(first_spawn_seq) if isinstance(first_spawn_seq, int) else None

    return {
        "goal": goal,
        "model_from_event": model,
        "rounds": total_rounds,
        "plan_updates": plan_updates,
        "first_plan_round": first_plan_round,
        "first_edit_round": first_edit_round,
        "parent_edit_count": parent_edits,
        "parent_edit_finished": parent_edit_finished,
        "parent_mutations": parent_mutations,
        "parent_tool_names": tool_names,
        "claim_count": claim_calls,
        "offer_round": offer_round,
        "offer_trigger": offer_trigger,
        "reoffer_round": reoffer_round,
        "kept_round": kept_round,
        "delegated_round": delegated_round,
        "first_spawn_round": first_spawn_round,
        "offered": "offered" in actions,
        "reoffered": "reoffered" in actions,
        "kept": kept,
        "delegated": delegated,
        "spawn": spawned,
        "delayed_spawn_after_keep": delayed_spawn_after_keep,
        "disposition": disposition,
        "engaged": engaged,
        "valid": engaged,
        "ownership_granted": ownership_granted,
        "ownership_denied": ownership_denied,
        "spawn_metric": spawn,
    }


def find_session_dbs(home: Path) -> list[Path]:
    if not home.exists():
        return []
    found = sorted(p for p in home.rglob("sessions.db") if p.is_file())
    out: list[Path] = []
    seen: set[Path] = set()
    for p in found:
        rp = p.resolve()
        if rp not in seen:
            seen.add(rp)
            out.append(rp)
    return out


def extract_path(db_path: str | Path) -> dict[str, Any]:
    path = str(db_path)
    con = connect_ro(path)
    try:
        data = extract_timeline(con)
        data["session_db"] = path
        return data
    finally:
        con.close()
