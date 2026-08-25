"""Multi-agent *value* metrics. Observer-only.

Primary estimand is task success and cost/quality against a single-agent
control. Spawn rate is recorded as a diagnostic, never as a success metric.
KEEP / no-spawn on the multi arm is a first-class outcome, not a fail.
"""

from __future__ import annotations

from typing import Any

from metrics import MIN_N_FOR_VERDICT, describe, rate


CHILD_CONTRIBUTIONS = (
    "exploration_reduction",
    "bug_found",
    "plan_improvement",
    "verification_improvement",
    "context_reduction",
)

# Criteria that can count as "an improvement". spawn_rate is intentionally
# absent: MA-WA1 showed spawn frequency ≠ value.
SUCCESS_METRICS = (
    "task_success",
    "turns",
    "tokens",
    "wall_time",
    "verification",
    "child_findings",
)

_FAIL_INTERPRETATION = (
    "no_measured_improvement_inspect_task_type_timing_child_usefulness_overhead"
)


def child_result_used(spawn: dict[str, Any]) -> bool:
    """True when the parent consumed at least one child's result.

    Worker mutations with granted ownership already count as `useful_child`.
    Explorers do not mutate, so a parent tool call or `resolve_finding` after
    `sub_agent_finished` is the consumption signal. A child that finished
    with no parent follow-up is not used.
    """
    if int(spawn.get("useful_child_count") or 0) > 0:
        return True
    if not (spawn.get("sub_agent_outcomes") or {}):
        return False
    if int(spawn.get("parent_resolve_finding_count") or 0) > 0:
        return True
    return bool(spawn.get("parent_tool_calls_after_child"))


def classify_child_contributions(spawn: dict[str, Any]) -> list[str]:
    """Heuristic labels from EventLog. Empty means unclassified, not 'none'.

    This is an observer projection, not a runtime claim. Human annotation
    overrides it. Labels outside CHILD_CONTRIBUTIONS are dropped.
    """
    children = spawn.get("children") or []
    outcomes = spawn.get("sub_agent_outcomes") or {}
    findings = {str(cid) for cid in (spawn.get("child_report_finding_ids") or [])}
    labels: list[str] = []

    explorer_consumed = False
    reviewer_done = False
    bug = False
    for child in children:
        cid = child.get("id")
        role = child.get("role")
        finished = cid in outcomes
        if role == "explorer" and finished and spawn.get("parent_mutations_after_child"):
            explorer_consumed = True
        if role == "reviewer" and finished:
            reviewer_done = True
        if cid is not None and str(cid) in findings and int(
            spawn.get("parent_resolve_finding_count") or 0
        ) > 0:
            bug = True

    if explorer_consumed:
        labels.append("exploration_reduction")
    if bug:
        labels.append("bug_found")
    if spawn.get("plan_updates_after_child") and outcomes:
        labels.append("plan_improvement")
    if reviewer_done:
        labels.append("verification_improvement")
    before = spawn.get("parent_reads_before_child")
    after = spawn.get("parent_reads_after_child")
    if (
        any(c.get("role") == "explorer" for c in children)
        and isinstance(before, int)
        and isinstance(after, int)
        and after < before
    ):
        labels.append("context_reduction")
    return [lab for lab in labels if lab in CHILD_CONTRIBUTIONS]


def _eff(run: dict[str, Any]) -> dict[str, Any]:
    return run.get("efficiency") or {}


def _ma(run: dict[str, Any]) -> dict[str, Any]:
    return run.get("multi_agent") or {}


def _col(runs: list[dict[str, Any]], getter) -> dict[str, float | int | None]:
    xs: list[float] = []
    for run in runs:
        value = getter(run)
        if isinstance(value, (int, float)):
            xs.append(float(value))
    return describe(xs)


def value_summary(runs: list[dict[str, Any]]) -> dict[str, Any]:
    scored = [r for r in runs if r.get("task_success") is not None]
    success_n = sum(1 for r in scored if r.get("task_success") is True)
    used_n = sum(1 for r in runs if _ma(r).get("child_result_used") or r.get("child_result_used"))
    spawn_n = sum(1 for r in runs if (r.get("metrics") or {}).get("spawn"))
    return {
        "n": len(runs),
        "n_scored": len(scored),
        "success_n": success_n,
        "success_rate": rate(success_n, len(scored)),
        "turns": _col(runs, lambda r: _eff(r).get("turns") or (r.get("edits") or {}).get("rounds")),
        "tokens": _col(runs, lambda r: _eff(r).get("total_tokens")),
        "wall_time_ms": _col(
            runs, lambda r: _eff(r).get("wall_time_ms") if _eff(r).get("wall_time_ms") is not None else _eff(r).get("duration")
        ),
        "tool_calls": _col(
            runs,
            lambda r: _eff(r).get("tool_calls")
            if _eff(r).get("tool_calls") is not None
            else (r.get("edits") or {}).get("parent_tool_calls"),
        ),
        "child_used_n": used_n,
        "child_used_rate": rate(used_n, len(runs)),
        "spawn_n": spawn_n,
        "insufficient_n": len(runs) < MIN_N_FOR_VERDICT,
    }


def _lower(new: float | None, old: float | None) -> bool:
    if new is None or old is None:
        return False
    return new < old


def _higher(new: float | None, old: float | None) -> bool:
    if new is None or old is None:
        return False
    return new > old


def compare_value_arms(
    control: list[dict[str, Any]],
    treatment: list[dict[str, Any]],
) -> dict[str, Any]:
    """Paired comparison. Does not compute or verdict on spawn rate."""
    a = value_summary(control)
    b = value_summary(treatment)
    improvements = {
        "turns": _lower(b["turns"]["mean"], a["turns"]["mean"]),
        "tokens": _lower(b["tokens"]["mean"], a["tokens"]["mean"]),
        "wall_time": _lower(b["wall_time_ms"]["mean"], a["wall_time_ms"]["mean"]),
        "verification": _higher(b["success_rate"], a["success_rate"]),
        "child_findings": (b["child_used_n"] or 0) > 0,
    }
    success_held = True
    if a["success_rate"] is not None and b["success_rate"] is not None:
        success_held = b["success_rate"] >= a["success_rate"]
    child_output_used = (b["child_used_n"] or 0) > 0
    insufficient = a["insufficient_n"] or b["insufficient_n"]
    return decide_value(
        {
            "control": a,
            "treatment": b,
            "success_held": success_held,
            "improvements": improvements,
            "child_output_used": child_output_used,
            "insufficient_n": insufficient,
            "min_n": MIN_N_FOR_VERDICT,
        }
    )


def decide_value(comparison: dict[str, Any]) -> dict[str, Any]:
    """Apply MA-VALUE-001 PASS/FAIL. Never concludes 'multi-agent has no value'."""
    out = dict(comparison)
    if comparison.get("insufficient_n"):
        out["verdict"] = "insufficient_n"
        out["interpretation"] = "insufficient_n"
        return out
    if not comparison.get("success_held"):
        out["verdict"] = "fail"
        out["interpretation"] = _FAIL_INTERPRETATION
        return out
    if not comparison.get("child_output_used"):
        out["verdict"] = "fail"
        out["interpretation"] = _FAIL_INTERPRETATION
        return out
    if not any((comparison.get("improvements") or {}).values()):
        out["verdict"] = "fail"
        out["interpretation"] = _FAIL_INTERPRETATION
        return out
    out["verdict"] = "pass"
    out["interpretation"] = "measurable_improvement"
    return out


def profile_effectiveness(spawn: dict[str, Any]) -> dict[str, Any]:
    """Per-profile observer metrics. Additive; never a success criterion by itself.

    Old EventLogs without `profile_id` fall back to `role`. Zeros mean
    "measured, contributed nothing"; a missing bucket means that profile
    did not spawn.
    """
    children = spawn.get("children") or []
    outcomes = spawn.get("sub_agent_outcomes") or {}
    useful = {str(cid) for cid in (spawn.get("useful_child_ids") or [])}
    buckets: dict[str, dict[str, Any]] = {}
    for child in children:
        pid = child.get("profile_id") or child.get("role") or "unknown"
        b = buckets.setdefault(
            pid,
            {
                "profile_id": pid,
                "profile_role": child.get("profile_role") or child.get("role"),
                "capabilities": list(child.get("capabilities") or []),
                "spawned": 0,
                "completed": 0,
                # A child whose terminal event carried no projection was never
                # observed. Counting it in `spawned` while adding 0 to the
                # finding sums dilutes every per-spawn rate with unmeasured
                # runs; these two make the denominator honest.
                "measured": 0,
                "unmeasured": 0,
                "findings_generated": 0,
                "findings_accepted": 0,
                "findings_verified": 0,
                "bugs_found": 0,
                "bugs_confirmed": 0,
                "changes_accepted": 0,
                "verification_passed": 0,
            },
        )
        if not b["capabilities"] and child.get("capabilities"):
            b["capabilities"] = list(child.get("capabilities") or [])
        b["spawned"] += 1
        cid = child.get("id")
        fin = outcomes.get(cid) or {}
        if fin.get("ok"):
            b["completed"] += 1
        contrib = fin.get("contribution")
        measured = isinstance(contrib, dict)
        # `null` means not measured, not "contributed nothing". Only the
        # finding sums are gated on it — `completed`, `changes_accepted` and
        # `verification_passed` are measured by other means and stay counted.
        b["measured" if measured else "unmeasured"] += 1
        gen = acc = ver = 0
        if measured:
            gen = int(contrib.get("findings_total") or 0)
            acc = int(contrib.get("findings_accepted") or 0)
            ver = int(contrib.get("findings_verified") or 0)
            b["findings_generated"] += gen
            b["findings_accepted"] += acc
            b["findings_verified"] += ver
        role = child.get("profile_role") or child.get("role")
        if role == "reviewer" and measured:
            b["bugs_found"] += gen
            b["bugs_confirmed"] += acc
        if role == "worker":
            if cid is not None and str(cid) in useful:
                b["changes_accepted"] += 1
            if fin.get("ok"):
                b["verification_passed"] += 1
    return buckets


def aggregate_profile_effectiveness(runs: list[dict[str, Any]]) -> dict[str, Any]:
    """Sum per-run profile buckets. Missing buckets stay missing."""
    out: dict[str, dict[str, Any]] = {}
    count_keys = (
        "spawned",
        "completed",
        "measured",
        "unmeasured",
        "findings_generated",
        "findings_accepted",
        "findings_verified",
        "bugs_found",
        "bugs_confirmed",
        "changes_accepted",
        "verification_passed",
    )
    for run in runs:
        buckets = (_ma(run).get("profile_effectiveness")
                   or (run.get("metrics") or {}).get("profile_effectiveness")
                   or {})
        if not isinstance(buckets, dict):
            continue
        for pid, raw in buckets.items():
            if not isinstance(raw, dict):
                continue
            b = out.setdefault(
                pid,
                {
                    "profile_id": pid,
                    "profile_role": raw.get("profile_role"),
                    "capabilities": list(raw.get("capabilities") or []),
                    **{k: 0 for k in count_keys},
                },
            )
            for k in count_keys:
                b[k] += int(raw.get(k) or 0)
    return out


def value_eval_result(run: dict[str, Any]) -> dict[str, Any]:
    """Compact projection matching the MA-VALUE-001 eval_result sketch.

    Additive: does not replace the observer run record.
    """
    eff = _eff(run)
    ma = _ma(run)
    tokens = eff.get("total_tokens")
    duration = eff.get("wall_time_ms")
    if duration is None:
        duration = eff.get("duration")
    turns = eff.get("turns")
    if turns is None:
        turns = (run.get("edits") or {}).get("rounds")
    roles = ma.get("child_roles")
    if not roles:
        roles = [c.get("role") for c in ((run.get("delegation") or {}).get("children") or [])]
    spawn_count = ma.get("spawn_count")
    if spawn_count is None:
        spawn_count = (run.get("delegation") or {}).get("natural_spawn_count", 0)
    return {
        "experiment": run.get("experiment"),
        "mode": run.get("mode"),
        "task_success": run.get("task_success"),
        "metrics": {
            "turns": turns,
            "tokens": tokens,
            "duration": duration,
        },
        "multi_agent": {
            "spawn_count": spawn_count,
            "child_roles": [r for r in (roles or []) if r is not None],
            "child_result_used": bool(ma.get("child_result_used")),
            "profile_effectiveness": ma.get("profile_effectiveness") or {},
        },
    }
