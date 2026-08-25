"""Reviewer *value* metrics. Observer-only.

Primary estimand is final correctness against a self-verify control.
Finding count is recorded, never a success metric. A reviewer that reports
nothing is valid. A reviewer that reports findings nobody judged is noise.
"""

from __future__ import annotations

from typing import Any

from metrics import MIN_N_FOR_VERDICT, describe, rate


# Success criteria. findings_generated is intentionally absent.
REVIEWER_SUCCESS_METRICS = (
    "task_success",
    "useful_findings",
    "verified_findings",
    "verification",
)

_FAIL_INTERPRETATION = (
    "no_measured_improvement_inspect_task_type_reviewer_usefulness_noise_overhead"
)


def extract_reviewer(spawn: dict[str, Any]) -> dict[str, Any]:
    """Project one run's independent-review contribution from spawn_metric.

    Joins `sub_agent_started` role=reviewer to `sub_agent_finished.contribution`.
    Missing contribution is zeros, not a skip — a reviewer that reported
    nothing is a measured zero.
    """
    children = [c for c in (spawn.get("children") or []) if c.get("role") == "reviewer"]
    outcomes = spawn.get("sub_agent_outcomes") or {}
    generated = accepted = verified = rejected = completed = 0
    measured = 0
    sources: list[str] = []
    for child in children:
        cid = child.get("id")
        finished = outcomes.get(cid) or {}
        if finished.get("ok"):
            completed += 1
        contrib = finished.get("contribution")
        if not isinstance(contrib, dict):
            # `null` is the runtime saying "not measured" — the independent
            # review stage runs outside the executor ledger and has no
            # projection to report. Reading it as zero fabricates a
            # "zero-finding reviewer" out of a reviewer that did report.
            continue
        measured += 1
        # `source` names the producing mechanism. Absent means the event
        # predates the stamp — "unknown", never inferred from the id shape.
        src = contrib.get("source")
        sources.append(str((src or {}).get("kind") or "unknown"))
        generated += int(contrib.get("findings_total") or 0)
        accepted += int(contrib.get("findings_accepted") or 0)
        verified += int(contrib.get("findings_verified") or 0)
        rejected += int(contrib.get("findings_rejected") or 0)
    unmeasured = bool(children) and measured == 0
    if unmeasured:
        return {
            "reviewer_spawned": len(children),
            "reviewer_completed": completed,
            "findings_generated": None,
            "findings_accepted": None,
            "findings_verified": None,
            "findings_rejected": None,
            "useful_findings": None,
            "zero_findings": False,
            "noise": False,
            "contribution_unmeasured": True,
            "contribution_sources": [],
        }
    useful = accepted
    judged = accepted + rejected
    return {
        "reviewer_spawned": len(children),
        "reviewer_completed": completed,
        "findings_generated": generated,
        "findings_accepted": accepted,
        "findings_verified": verified,
        "findings_rejected": rejected,
        "useful_findings": useful,
        "zero_findings": bool(children) and generated == 0,
        "noise": generated > 0 and judged == 0,
        "contribution_unmeasured": False,
        "contribution_sources": sources,
    }


def reviewer_summary(runs: list[dict[str, Any]]) -> dict[str, Any]:
    scored = [r for r in runs if r.get("task_success") is not None]
    success_n = sum(1 for r in scored if r.get("task_success") is True)
    spawned_n = sum(1 for r in runs if int(_rev(r).get("reviewer_spawned") or 0) > 0)
    useful_n = sum(1 for r in runs if int(_rev(r).get("useful_findings") or 0) > 0)
    unmeasured_n = sum(1 for r in runs if _rev(r).get("contribution_unmeasured"))
    noise_n = sum(1 for r in runs if _rev(r).get("noise"))
    zero_n = sum(1 for r in runs if _rev(r).get("zero_findings"))
    return {
        "n": len(runs),
        "n_scored": len(scored),
        "success_n": success_n,
        "success_rate": rate(success_n, len(scored)),
        "reviewer_spawned_n": spawned_n,
        "reviewer_spawned_rate": rate(spawned_n, len(runs)),
        "useful_findings_n": useful_n,
        "zero_findings_n": zero_n,
        "noise_n": noise_n,
        "contribution_unmeasured_n": unmeasured_n,
        "findings_generated": _col(runs, lambda r: _rev(r).get("findings_generated")),
        "findings_accepted": _col(runs, lambda r: _rev(r).get("findings_accepted")),
        "findings_verified": _col(runs, lambda r: _rev(r).get("findings_verified")),
        "turns": _col(runs, lambda r: _eff(r).get("turns") or (r.get("edits") or {}).get("rounds")),
        "tokens": _col(runs, lambda r: _eff(r).get("total_tokens")),
        "wall_time_ms": _col(
            runs,
            lambda r: _eff(r).get("wall_time_ms")
            if _eff(r).get("wall_time_ms") is not None
            else _eff(r).get("duration"),
        ),
        "tool_calls": _col(
            runs,
            lambda r: _eff(r).get("tool_calls")
            if _eff(r).get("tool_calls") is not None
            else (r.get("edits") or {}).get("parent_tool_calls"),
        ),
        "insufficient_n": len(runs) < MIN_N_FOR_VERDICT,
    }


def compare_reviewer_arms(
    control: list[dict[str, Any]],
    treatment: list[dict[str, Any]],
) -> dict[str, Any]:
    """Paired comparison. Finding count is not a success criterion."""
    a = reviewer_summary(control)
    b = reviewer_summary(treatment)
    success_held = True
    if a["success_rate"] is not None and b["success_rate"] is not None:
        success_held = b["success_rate"] >= a["success_rate"]
    quality_improved = _higher(b["success_rate"], a["success_rate"])
    useful = (b["useful_findings_n"] or 0) > 0
    noise_regression = (b["noise_n"] or 0) > (a["noise_n"] or 0) and not useful
    reviewer_ran = (b["reviewer_spawned_n"] or 0) > 0
    insufficient = a["insufficient_n"] or b["insufficient_n"]
    return decide_reviewer(
        {
            "control": a,
            "treatment": b,
            "success_held": success_held,
            "quality_improved": quality_improved,
            "useful_findings": useful,
            "reviewer_ran": reviewer_ran,
            "noise_regression": noise_regression,
            "insufficient_n": insufficient,
            "min_n": MIN_N_FOR_VERDICT,
            "cost": {
                "turns": {
                    "control": a["turns"]["mean"],
                    "treatment": b["turns"]["mean"],
                },
                "tokens": {
                    "control": a["tokens"]["mean"],
                    "treatment": b["tokens"]["mean"],
                },
                "wall_time_ms": {
                    "control": a["wall_time_ms"]["mean"],
                    "treatment": b["wall_time_ms"]["mean"],
                },
            },
        }
    )


def decide_reviewer(comparison: dict[str, Any]) -> dict[str, Any]:
    """Pilot/formal verdict. Never concludes 'reviewer has no value'."""
    out = dict(comparison)
    if comparison.get("insufficient_n"):
        out["verdict"] = "insufficient_n"
        out["interpretation"] = "insufficient_n"
        return out
    if not comparison.get("success_held"):
        out["verdict"] = "fail"
        out["interpretation"] = _FAIL_INTERPRETATION
        return out
    if comparison.get("noise_regression"):
        out["verdict"] = "fail"
        out["interpretation"] = _FAIL_INTERPRETATION
        return out
    if not comparison.get("reviewer_ran"):
        out["verdict"] = "fail"
        out["interpretation"] = "reviewer_did_not_run"
        return out
    if comparison.get("quality_improved") or comparison.get("useful_findings"):
        out["verdict"] = "pass"
        out["interpretation"] = "measurable_improvement"
        return out
    # Reviewer ran, success held, zero useful findings: valid, not a pass.
    out["verdict"] = "fail"
    out["interpretation"] = _FAIL_INTERPRETATION
    return out


def reviewer_eval_result(run: dict[str, Any]) -> dict[str, Any]:
    """Compact projection. Additive on the observer run record."""
    eff = _eff(run)
    rev = _rev(run)
    return {
        "experiment": run.get("experiment"),
        "mode": run.get("mode"),
        "task_success": run.get("task_success"),
        "metrics": {
            "turns": eff.get("turns") or (run.get("edits") or {}).get("rounds"),
            "tokens": eff.get("total_tokens"),
            "duration": eff.get("wall_time_ms")
            if eff.get("wall_time_ms") is not None
            else eff.get("duration"),
        },
        "reviewer": {
            "spawned": int(rev.get("reviewer_spawned") or 0),
            "completed": int(rev.get("reviewer_completed") or 0),
            "findings_generated": int(rev.get("findings_generated") or 0),
            "findings_accepted": int(rev.get("findings_accepted") or 0),
            "findings_verified": int(rev.get("findings_verified") or 0),
            "useful_findings": int(rev.get("useful_findings") or 0),
            "zero_findings": bool(rev.get("zero_findings")),
            "noise": bool(rev.get("noise")),
        },
    }


def _rev(run: dict[str, Any]) -> dict[str, Any]:
    return run.get("reviewer") or {}


def _eff(run: dict[str, Any]) -> dict[str, Any]:
    return run.get("efficiency") or {}


def _col(runs: list[dict[str, Any]], getter) -> dict[str, float | int | None]:
    xs: list[float] = []
    for run in runs:
        value = getter(run)
        if isinstance(value, (int, float)):
            xs.append(float(value))
    return describe(xs)


def _higher(new: float | None, old: float | None) -> bool:
    if new is None or old is None:
        return False
    return new > old
