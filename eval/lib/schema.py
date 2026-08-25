"""Unified eval result schema (observer records, not product state)."""

from __future__ import annotations

from typing import Any

SCHEMA_VERSION = "1"

RUN_REQUIRED = (
    "schema_version",
    "run",
    "task",
    "arm",
    "model",
    "delegation",
    "edits",
    "verifier",
    "safety",
    "metrics",
)

BATCH_REQUIRED = ("schema_version", "batch_id", "runs")


def _require(obj: dict[str, Any], keys: tuple[str, ...], where: str) -> list[str]:
    missing = [k for k in keys if k not in obj]
    return [f"{where}: missing {k}" for k in missing]


def validate_run(doc: dict[str, Any]) -> list[str]:
    errors = _require(doc, RUN_REQUIRED, "run")
    if doc.get("schema_version") != SCHEMA_VERSION:
        errors.append(f"run: schema_version must be {SCHEMA_VERSION!r}")
    for section in ("run", "task", "arm", "model", "delegation", "edits", "verifier", "safety", "metrics"):
        if section in doc and not isinstance(doc[section], dict):
            errors.append(f"{section}: must be an object")
    return errors


def validate_batch(doc: dict[str, Any]) -> list[str]:
    errors = _require(doc, BATCH_REQUIRED, "batch")
    if doc.get("schema_version") != SCHEMA_VERSION:
        errors.append(f"batch: schema_version must be {SCHEMA_VERSION!r}")
    runs = doc.get("runs")
    if not isinstance(runs, list):
        errors.append("batch: runs must be an array")
        return errors
    for i, run in enumerate(runs):
        if not isinstance(run, dict):
            errors.append(f"batch.runs[{i}]: must be an object")
            continue
        errors.extend(f"batch.runs[{i}].{e}" for e in validate_run(run))
    return errors


def _decision(timeline: dict[str, Any]) -> tuple[int | None, int | None]:
    offer = timeline.get("offer_round")
    rounds: list[int] = []
    for key in ("kept_round", "delegated_round", "first_spawn_round"):
        value = timeline.get(key)
        if isinstance(value, int):
            rounds.append(value)
    if not rounds:
        return None, None
    decision_round = min(rounds)
    latency = None
    if isinstance(offer, int):
        latency = decision_round - offer
    return decision_round, latency


def compact_record(run: dict[str, Any]) -> dict[str, Any]:
    """Flat observer record for the decision benchmark (KEEP is a disposition, not a fail)."""
    d = run.get("delegation") or {}
    e = run.get("edits") or {}
    m = run.get("metrics") or {}
    t = run.get("task") or {}
    s = run.get("safety") or {}
    v = run.get("verifier") or {}
    return {
        "run_id": (run.get("run") or {}).get("id"),
        "task": t.get("id"),
        "shape": t.get("shape"),
        "model": (run.get("model") or {}).get("ref"),
        "offer_seen": bool(d.get("offered") or m.get("offer_seen")),
        "delegation": {
            "spawn": bool(m.get("spawn")),
            "worker_count": d.get("natural_spawn_count", 0),
            "decision_round": m.get("decision_round"),
            "decision_latency_rounds": m.get("decision_latency_rounds"),
            "disposition": m.get("disposition"),
        },
        "execution": {
            "turns": e.get("rounds"),
            "edits": e.get("parent_edit_count"),
            "verifier": v.get("passed"),
        },
        "safety": {
            "violations": s.get("violations", 0),
            "ownership_denied": s.get("ownership_denied", 0),
        },
    }


def make_run(
    *,
    run_id: str,
    started_at: str | None,
    git_sha: str | None,
    binary: str | None,
    leveler_home: str | None,
    session_db: str | None,
    task_id: str,
    suite: str,
    max_rounds: int | None,
    expected_disposition: str | None,
    arm_name: str,
    arm_factor: str,
    arm_value: str,
    model_ref: str | None,
    timeline: dict[str, Any],
    verifier_ran: bool = False,
    verifier_passed: bool | None = None,
    verifier_command: str | None = None,
    shape: str | None = None,
    experiment: str | None = None,
    mode: str | None = None,
) -> dict[str, Any]:
    spawn = timeline.get("spawn_metric") or {}
    decision_round, latency = _decision(timeline)
    offered = bool(timeline.get("offered"))
    spawned = bool(timeline.get("spawn"))
    doc = {
        "schema_version": SCHEMA_VERSION,
        "run": {
            "id": run_id,
            "started_at": started_at,
            "git_sha": git_sha,
            "binary": binary,
            "leveler_home": leveler_home,
            "session_db": session_db or timeline.get("session_db"),
        },
        "task": {
            "id": task_id,
            "suite": suite,
            "max_rounds": max_rounds,
            "expected_disposition": expected_disposition,
            "shape": shape,
        },
        "arm": {
            "name": arm_name,
            "factor": arm_factor,
            "value": arm_value,
        },
        "model": {
            "ref": model_ref or timeline.get("model_from_event"),
        },
        "delegation": {
            "offered": bool(timeline.get("offered")),
            "offer_trigger": timeline.get("offer_trigger"),
            "offer_round": timeline.get("offer_round"),
            "reoffered": bool(timeline.get("reoffered")),
            "reoffer_round": timeline.get("reoffer_round"),
            "kept": bool(timeline.get("kept")),
            "kept_round": timeline.get("kept_round"),
            "delegated": bool(timeline.get("delegated")),
            "delegated_round": timeline.get("delegated_round"),
            "natural_spawn_count": spawn.get("natural_spawn_count", 0),
            "useful_child_count": spawn.get("useful_child_count", 0),
            "reviewer_children": spawn.get("reviewer_children", 0),
            "children": spawn.get("children", []),
            "stages": spawn.get("delegation_stages", []),
            "delayed_spawn_after_keep": bool(timeline.get("delayed_spawn_after_keep")),
        },
        "edits": {
            "first_edit_round": timeline.get("first_edit_round"),
            "first_plan_round": timeline.get("first_plan_round"),
            "parent_edit_count": timeline.get("parent_edit_count", 0),
            "parent_mutations": timeline.get("parent_mutations", 0),
            "parent_tool_calls": spawn.get("parent_tool_calls", 0),
            "rounds": timeline.get("rounds", 0),
            "plan_updates": timeline.get("plan_updates", 0),
            "claim_count": timeline.get("claim_count", 0),
        },
        "verifier": {
            "ran": verifier_ran,
            "passed": verifier_passed,
            "command": verifier_command,
        },
        "safety": {
            "ownership_granted": timeline.get("ownership_granted", 0),
            "ownership_denied": timeline.get("ownership_denied", 0),
            "claim_count": timeline.get("claim_count", 0),
            "violations": 0,
        },
        "metrics": {
            "valid": bool(timeline.get("valid")),
            "engaged": bool(timeline.get("engaged")),
            "spawn": spawned,
            "offer_seen": offered,
            "decision_round": decision_round,
            "decision_latency_rounds": latency,
            "disposition": timeline.get("disposition"),
        },
    }
    if experiment:
        doc["experiment"] = experiment
    if mode:
        doc["mode"] = mode
    if experiment or mode:
        used = timeline.get("child_result_used")
        if used is None:
            used = bool(spawn.get("useful_child_count")) or bool(
                spawn.get("parent_tool_calls_after_child")
            ) or int(spawn.get("parent_resolve_finding_count") or 0) > 0
        roles = [c.get("role") for c in (spawn.get("children") or []) if c.get("role")]
        outcomes = spawn.get("sub_agent_outcomes") or {}
        doc["task_success"] = verifier_passed if verifier_ran else None
        doc["efficiency"] = {
            "turns": timeline.get("rounds", 0),
            "input_tokens": timeline.get("input_tokens"),
            "output_tokens": timeline.get("output_tokens"),
            "total_tokens": timeline.get("total_tokens"),
            "wall_time_ms": timeline.get("wall_time_ms"),
            "tool_calls": spawn.get("parent_tool_calls", timeline.get("parent_tool_calls", 0)),
        }
        doc["multi_agent"] = {
            "spawn_count": spawn.get("natural_spawn_count", 0),
            "child_roles": roles,
            "child_completed": len(outcomes),
            "child_result_used": bool(used),
            "child_contributions": timeline.get("child_contributions") or [],
            "children": spawn.get("children") or [],
            "profile_effectiveness": timeline.get("profile_effectiveness") or {},
        }
        quality = timeline.get("quality")
        if not isinstance(quality, dict):
            quality = {
                "tests_passed": timeline.get("tests_passed"),
                "regressions": timeline.get("regressions"),
                "review_findings": timeline.get("review_findings"),
                "missed_issues": timeline.get("missed_issues"),
            }
        doc["quality"] = quality
        if timeline.get("reviewer") is not None:
            doc["reviewer"] = timeline.get("reviewer")
    return doc


def make_batch(
    *,
    batch_id: str,
    runs: list[dict[str, Any]],
    arm: dict[str, Any] | None = None,
    model: str | None = None,
    notes: str | None = None,
) -> dict[str, Any]:
    doc = {
        "schema_version": SCHEMA_VERSION,
        "batch_id": batch_id,
        "arm": arm,
        "model": model,
        "notes": notes,
        "runs": runs,
    }
    errors = validate_batch(doc)
    if errors:
        raise ValueError("invalid batch: " + "; ".join(errors))
    return doc
